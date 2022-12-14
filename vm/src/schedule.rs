use {
  crate::{
    execute,
    execution,
    state::Overlayed,
    syncell::SynCell,
    State,
    StateDiff,
  },
  anoma_primitives::{Address, Code, Param, Transaction},
  petgraph::{
    dot,
    prelude::DiGraph,
    stable_graph::NodeIndex,
    unionfind::UnionFind,
    visit::{
      EdgeRef,
      GraphRef,
      IntoNeighbors,
      NodeIndexable,
      VisitMap,
      Visitable,
    },
    Direction,
  },
  rayon::prelude::*,
  std::collections::{HashSet, VecDeque},
};

/// Runs multiple transactions in parallel, while preserving read/write
/// dependency ordering. This function is usually called on all transactions
/// within one block in the blockchain.
///
/// Produces a list of results that contain either a state diff on successfull
/// transaction execution or an error explaining why a tx failed. The resulting
/// collection of results is in the same order as the input txs.
pub fn execute_many(
  state: &dyn State,
  cache: &dyn State,
  txs: impl Iterator<Item = Transaction>,
) -> Vec<Result<StateDiff, execution::Error>> {
  Schedule::new(state, txs).run(state, cache).collect()
}

type NodeType = SynCell<Option<(Transaction, usize)>>;

struct Schedule {
  graph: DiGraph<NodeType, ()>,
  roots: Vec<NodeIndex>,
}

struct Tree<'s> {
  schedule: &'s Schedule,
  root: NodeIndex,
}

impl<'s> std::fmt::Debug for Tree<'s> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Tree")
      .field("graph", &"[schedule]")
      .field("root", &self.root)
      .finish()
  }
}

impl<'s> Tree<'s> {
  pub fn run(
    self,
    state: &dyn State,
    cache: &dyn State,
  ) -> impl Iterator<Item = (Result<StateDiff, execution::Error>, usize)> {
    let mut txs = vec![];
    let mut acc_state = StateDiff::default();
    let mut iter = BfsRows::new(&self.schedule.graph, self.root);
    while let Some(row) = iter.next(&self.schedule.graph) {
      // gather all txs belonging to the same deptree row,
      // and remove them from the full depgraph.
      let row_txs: Vec<_> = row
        .into_iter()
        .map(|tx| {
          self
            .schedule
            .graph
            .node_weight(tx)
            .expect("retreived through traversal")
            .borrow_mut()
            .take()
            .expect("transaction visited more than once")
        })
        .collect();

      // always keep track of the transaction original position in the block,
      // later all execution results will be ordered in the same order as they
      // appear in the block.
      //
      // Run all txs on the same level in parallel:
      let state = Overlayed::new(state, &acc_state);
      let results: Vec<_> = row_txs
        .into_par_iter()
        .map(|(tx, ix)| (execute(tx, &state, cache), ix))
        .collect();

      // accumulate state changes within one tx dependency tree row,
      // the next row of this dependency tree needs to see mutations
      // resulting from previous txs.
      for (result, ix) in results {
        if let Ok(ref diff) = result {
          acc_state.apply(diff.clone());
        }

        txs.push((result, ix));
      }
    }

    txs.into_iter()
  }
}

impl Schedule {
  pub fn new(
    state: &dyn State,
    txs: impl Iterator<Item = Transaction>,
  ) -> Self {
    let mut graph = DiGraph::new();
    let mut refs: VecDeque<(_, _)> = txs
      .enumerate()
      .map(|(ix, tx)| {
        (
          TransactionRefs::new(&tx, state),
          graph.add_node(SynCell::new(Some((tx, ix)))),
        )
      })
      .collect();

    // identify all r/w dependencies for this tx ordering
    while let Some((r0, ix0)) = refs.pop_back() {
      'inner: for (r1, ix1) in refs.iter().rev() {
        if r0.depends_on(r1) {
          graph.add_edge(*ix1, ix0, ());
          break 'inner;
        }
      }
    }

    Self {
      roots: Self::roots(&graph),
      graph,
    }
  }

  pub fn run(
    self,
    state: &dyn State,
    cache: &dyn State,
  ) -> impl Iterator<Item = Result<StateDiff, execution::Error>> {
    let mut trees: Vec<(Result<StateDiff, execution::Error>, usize)> = self
      .trees()
      .into_par_iter()
      .map(|tree: Tree| tree.run(state, cache).collect::<Vec<_>>())
      .flatten()
      .collect();

    trees.sort_by(|(_, ix1), (_, ix2)| ix1.cmp(ix2));
    trees.into_iter().map(|(tx, _)| tx)
  }

  /// Returns all independent dependency trees in the schedule
  /// deps graph. Each tree is safe to be scheduled in parallel
  /// with other trees.
  fn trees(&self) -> Vec<Tree<'_>> {
    self
      .roots
      .iter()
      .map(|r| Tree {
        schedule: self,
        root: *r,
      })
      .collect()
  }

  /// This function identifies independent disjoint trees in
  /// the tx dependency graph. Those trees can be scheduled in parallel.
  /// The returned nodes are roots of every identified dependency tree
  /// in the transaction list.
  fn roots(graph: &DiGraph<NodeType, ()>) -> Vec<NodeIndex> {
    let mut vertex_sets = UnionFind::new(graph.node_bound());
    for edge in graph.edge_references() {
      let (a, b) = (edge.source(), edge.target());
      vertex_sets.union(graph.to_index(a), graph.to_index(b));
    }

    // the labels vector is a list of nodes, that is equal in length
    // to the number of independent trees in the graph with a node
    // from each tree in the "lables" vector.
    let mut labels = vertex_sets.into_labeling();
    labels.sort_unstable();
    labels.dedup();

    // now find the root of each tree
    let mut roots = Vec::with_capacity(labels.len());
    for label in labels {
      let mut index = graph.from_index(label);
      while let Some(up) = // follow edges until root
        graph.edges_directed(index, Direction::Incoming).last()
      {
        index = up.source()
      }
      roots.push(index);
    }

    roots
  }
}

impl std::fmt::Debug for Schedule {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Schedule")
      .field(
        "graph",
        &dot::Dot::with_config(&self.graph, &[
          dot::Config::EdgeNoLabel,
          dot::Config::NodeIndexLabel,
        ]),
      )
      .field("roots", &self.roots)
      .finish()
  }
}

/// Specifies the list of all accounts that a transaction will read or write to.
/// This is used when scheduling transactions for execution in parallel.
#[derive(Debug, PartialEq, Eq)]
struct TransactionRefs {
  reads: HashSet<Address>,
  writes: HashSet<Address>,
}

impl TransactionRefs {
  pub fn depends_on(&self, other: &Self) -> bool {
    self.reads.iter().any(|addr| other.writes.contains(addr))
      || self.writes.iter().any(|addr| other.writes.contains(addr))
  }

  pub fn new(tx: &Transaction, state: &dyn State) -> Self {
    let mut reads = HashSet::new();
    let mut writes = HashSet::new();

    // collect all writes
    for addr in tx.proposals.keys() {
      // add the account that we want to mutate
      writes.insert(addr.clone());
    }

    // if an account is both read and write, then
    // it belongs to the "write" subset, because
    // it is what matters when locking state and
    // scheduling concurrent executions of transactions.

    // collect all reads that will occur when evaluating
    // the validity predicates of the mutatated account and
    // all its ancestors.
    for addr in tx.proposals.keys() {
      // and all references used by its predicates
      if let Some(acc) = state.get(addr) {
        acc.predicates.for_each(&mut |pred| {
          for param in &pred.params {
            if let Param::AccountRef(addr) = param {
              if !writes.contains(addr) {
                reads.insert(addr.clone());
              }
            };
          }

          if let Code::AccountRef(ref addr, _) = pred.code {
            if !writes.contains(addr) {
              reads.insert(addr.clone());
            }
          }
        });

        // then all references used by predicates of all its ancestors
        for ancestor in addr.ancestors() {
          if let Some(acc) = state.get(&ancestor) {
            acc.predicates.for_each(&mut |pred| {
              for param in &pred.params {
                if let Param::AccountRef(addr) = param {
                  if !writes.contains(addr) {
                    reads.insert(addr.clone());
                  }
                };
              }
              if let Code::AccountRef(ref addr, _) = pred.code {
                if !writes.contains(addr) {
                  reads.insert(addr.clone());
                }
              }
            });
          }
        }
      }
    }

    // collect all reads that will occur when evaluating
    // intent predicates.
    for intent in &tx.intents {
      intent.expectations.for_each(&mut |pred| {
        for param in &pred.params {
          if let Param::AccountRef(addr) = param {
            if !writes.contains(addr) {
              reads.insert(addr.clone());
            }
          };
        }

        if let Code::AccountRef(ref addr, _) = pred.code {
          if !writes.contains(addr) {
            reads.insert(addr.clone());
          }
        }
      })
    }

    Self { reads, writes }
  }
}

#[derive(Clone)]
struct BfsRows<N, VM> {
  stack: VecDeque<(N, usize)>,
  discovered: VM,
  row: usize,
}

impl<N, VM: Default> Default for BfsRows<N, VM> {
  fn default() -> Self {
    Self {
      stack: VecDeque::new(),
      discovered: VM::default(),
      row: 0,
    }
  }
}

impl<N, VM> BfsRows<N, VM>
where
  N: Copy + PartialEq,
  VM: VisitMap<N>,
{
  pub fn new<G>(graph: G, start: N) -> Self
  where
    G: GraphRef + Visitable<NodeId = N, Map = VM>,
  {
    let row = 0;
    let mut stack = VecDeque::new();
    let mut discovered = graph.visit_map();
    discovered.visit(start);
    stack.push_front((start, row));
    BfsRows {
      stack,
      discovered,
      row,
    }
  }

  pub fn next<G>(&mut self, graph: G) -> Option<Vec<N>>
  where
    G: IntoNeighbors<NodeId = N>,
  {
    let mut row = vec![];
    while let Some((node, level)) = self.stack.pop_front() {
      for succ in graph.neighbors(node) {
        if self.discovered.visit(succ) {
          self.stack.push_back((succ, level + 1));
        }
      }

      if level == self.row {
        row.push(node);
      } else {
        // belongs to a different level, put it back on stack.
        // Will be picked up on next call to this method for the
        // next row.
        self.stack.push_front((node, level));
        break;
      }
    }

    self.row += 1;

    match row.len() {
      0 => return None,
      _ => Some(row),
    }
  }
}
