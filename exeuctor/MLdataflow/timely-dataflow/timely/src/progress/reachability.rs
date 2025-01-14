// READ, Sep 12 2021
//! Manages pointstamp reachability within a timely dataflow graph.
//!
//! Timely dataflow is concerned with understanding and communicating the potential
//! for capabilities to reach nodes in a directed graph, by following paths through
//! the graph (along edges and through nodes). This module contains one abstraction
//! for managing this information.
//!
//! # Examples
//!
//! ```rust
//! use timely::progress::{Location, Port};
//! use timely::progress::frontier::Antichain;
//! use timely::progress::{Source, Target};
//! use timely::progress::reachability::{Builder, Tracker};
//!
//! // allocate a new empty topology builder.
//! let mut builder = Builder::<usize>::new();
//!
//! // Each node with one input connected to one output.
//! builder.add_node(0, 1, 1, vec![vec![Antichain::from_elem(0)]]);
//! builder.add_node(1, 1, 1, vec![vec![Antichain::from_elem(0)]]);
//! builder.add_node(2, 1, 1, vec![vec![Antichain::from_elem(1)]]);
//!
//! // Connect nodes in sequence, looping around to the first from the last.
//! builder.add_edge(Source::new(0, 0), Target::new(1, 0));
//! builder.add_edge(Source::new(1, 0), Target::new(2, 0));
//! builder.add_edge(Source::new(2, 0), Target::new(0, 0));
//!
//! // Construct a reachability tracker.
//! let (mut tracker, _) = builder.build();
//!
//! // Introduce a pointstamp at the output of the first node.
//! tracker.update_source(Source::new(0, 0), 17, 1);
//!
//! // Propagate changes; until this call updates are simply buffered.
//! tracker.propagate_all();
//!
//! let mut results =
//! tracker
//!     .pushed()
//!     .drain()
//!     .filter(|((location, time), delta)| location.is_target())
//!     .collect::<Vec<_>>();
//!
//! results.sort();
//!
//! println!("{:?}", results);
//!
//! assert_eq!(results.len(), 3);
//! assert_eq!(results[0], ((Location::new_target(0, 0), 18), 1));
//! assert_eq!(results[1], ((Location::new_target(1, 0), 17), 1));
//! assert_eq!(results[2], ((Location::new_target(2, 0), 17), 1));
//!
//! // Introduce a pointstamp at the output of the first node.
//! tracker.update_source(Source::new(0, 0), 17, -1);
//!
//! // Propagate changes; until this call updates are simply buffered.
//! tracker.propagate_all();
//!
//! let mut results =
//! tracker
//!     .pushed()
//!     .drain()
//!     .filter(|((location, time), delta)| location.is_target())
//!     .collect::<Vec<_>>();
//!
//! results.sort();
//!
//! assert_eq!(results.len(), 3);
//! assert_eq!(results[0], ((Location::new_target(0, 0), 18), -1));
//! assert_eq!(results[1], ((Location::new_target(1, 0), 17), -1));
//! assert_eq!(results[2], ((Location::new_target(2, 0), 17), -1));
//! ```

use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::cmp::Reverse;

use crate::progress::Timestamp;
use crate::progress::{Source, Target};
use crate::progress::ChangeBatch;
use crate::progress::{Location, Port};

use crate::progress::frontier::{Antichain, MutableAntichain};
use crate::progress::timestamp::PathSummary;


/// A topology builder, which can summarize reachability along paths.
///
/// A `Builder` takes descriptions of the nodes and edges in a graph, and compiles
/// a static summary of the minimal actions a timestamp must endure going from any
/// input or output port to a destination input port.
///
/// A graph is provides as (i) several indexed nodes, each with some number of input
/// and output ports, and each with a summary of the internal paths connecting each
/// input to each output, and (ii) a set of edges connecting output ports to input
/// ports. Edges do not adjust timestamps; only nodes do this.
///
/// The resulting summary describes, for each origin port in the graph and destination
/// input port, a set of incomparable path summaries, each describing what happens to
/// a timestamp as it moves along the path. There may be multiple summaries for each
/// part of origin and destination due to the fact that the actions on timestamps may
/// not be totally ordered (e.g., "increment the timestamp" and "take the maximum of
/// the timestamp and seven").
///
/// # Examples
///
/// ```rust
/// use timely::progress::frontier::Antichain;
/// use timely::progress::{Source, Target};
/// use timely::progress::reachability::Builder;
///
/// // allocate a new empty topology builder.
/// let mut builder = Builder::<usize>::new();
///
/// // Each node with one input connected to one output.
/// builder.add_node(0, 1, 1, vec![vec![Antichain::from_elem(0)]]);
/// builder.add_node(1, 1, 1, vec![vec![Antichain::from_elem(0)]]);
/// builder.add_node(2, 1, 1, vec![vec![Antichain::from_elem(1)]]);
///
/// // Connect nodes in sequence, looping around to the first from the last.
/// builder.add_edge(Source::new(0, 0), Target::new(1, 0));
/// builder.add_edge(Source::new(1, 0), Target::new(2, 0));
/// builder.add_edge(Source::new(2, 0), Target::new(0, 0));
///
/// // Summarize reachability information.
/// let (tracker, _) = builder.build();
/// ```
#[derive(Clone, Debug)]
pub struct Builder<T: Timestamp> {
    /// Internal connections within hosted operators.
    ///
    /// Indexed by operator index, then input port, then output port. This is the
    /// same format returned by `get_internal_summary`, as if we simply appended
    /// all of the summaries for the hosted nodes.
    // there may be multiple minimal, incomparable PathSummaries between an input and an output port
    // also between two ports in the scope / subgraph
    // since PathSummary is only PartialOrder
    // there might be a path that increase the timestamp by 5
    // and there is another path that just sets the timestamp to max(input_timestamp, 7)
    pub nodes: Vec<Vec<Vec<Antichain<T::Summary>>>>,
    /// Direct connections from sources to targets.
    ///
    /// Edges do not affect timestamps, so we only need to know the connectivity.
    /// Indexed by operator index then output port.
    pub edges: Vec<Vec<Vec<Target>>>,
    /// Numbers of inputs and outputs for each node.
    pub shape: Vec<(usize, usize)>,
}

impl<T: Timestamp> Builder<T> {

    /// Create a new empty topology builder.
    pub fn new() -> Self {
        Builder {
            nodes: Vec::new(),
            edges: Vec::new(),
            shape: Vec::new(),
        }
    }

    /// Add links internal to operators.
    ///
    /// This method overwrites any existing summary, instead of anything more sophisticated.
    pub fn add_node(&mut self, index: usize, inputs: usize, outputs: usize, summary: Vec<Vec<Antichain<T::Summary>>>) {
        // add an operator node with several input ports and several output ports
        // summary[i, j] is the PathSummary of the internal path (operation performed by this node)
        // from input port i -> output port j
        // NOTE: only nodes (operators) change timestamps, but not edges

        // Assert that all summaries exist.
        debug_assert_eq!(inputs, summary.len());
        for x in summary.iter() { debug_assert_eq!(outputs, x.len()); }

        // allocate storage
        while self.nodes.len() <= index {
            self.nodes.push(Vec::new());
            self.edges.push(Vec::new());
            self.shape.push((0, 0));
        }

        // push internal path summaries
        self.nodes[index] = summary;
        if self.edges[index].len() != outputs {
            // self.edges[index][p] is the vector of outgoing edges for output port p (source)
            // self.edges[index] is of length #outputs_ports
            self.edges[index] = vec![Vec::new(); outputs];
        }
        self.shape[index] = (inputs, outputs);
    }

    /// Add links between operators.
    ///
    /// This method does not check that the associated nodes and ports exist. References to
    /// missing nodes or ports are discovered in `build`.
    pub fn add_edge(&mut self, source: Source, target: Target) {
        // each input port can be connected to a single output port
        // source: output port
        // target: input port
        // we are connecting source -> target

        // Assert that the edge is between existing ports.
        // shape is (#inputs, #outputs)
        debug_assert!(source.port < self.shape[source.node].1);
        debug_assert!(target.port < self.shape[target.node].0);

        self.edges[source.node][source.port].push(target);
    }

    /// Compiles the current nodes and edges into immutable path summaries.
    ///
    /// This method has the opportunity to perform some error checking that the path summaries
    /// are valid, including references to undefined nodes and ports, as well as self-loops with
    /// default summaries (a serious liveness issue).
    pub fn build(&self) -> (Tracker<T>, Vec<Vec<Antichain<T::Summary>>>) {

        if !self.is_acyclic() {
            println!("Cycle detected without timestamp increment");
            println!("{:?}", self);
        }

        Tracker::allocate_from(self)
    }

    /// Tests whether the graph a cycle of default path summaries.
    ///
    /// Graphs containing cycles of default path summaries will most likely
    /// not work well with progress tracking, as a timestamp can result in
    /// itself. Such computations can still *run*, but one should not block
    /// on frontier information before yielding results, as you many never
    /// unblock.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use timely::progress::frontier::Antichain;
    /// use timely::progress::{Source, Target};
    /// use timely::progress::reachability::Builder;
    ///
    /// // allocate a new empty topology builder.
    /// let mut builder = Builder::<usize>::new();
    ///
    /// // Each node with one input connected to one output.
    /// builder.add_node(0, 1, 1, vec![vec![Antichain::from_elem(0)]]);
    /// builder.add_node(1, 1, 1, vec![vec![Antichain::from_elem(0)]]);
    /// builder.add_node(2, 1, 1, vec![vec![Antichain::from_elem(0)]]);
    ///
    /// // Connect nodes in sequence, looping around to the first from the last.
    /// builder.add_edge(Source::new(0, 0), Target::new(1, 0));
    /// builder.add_edge(Source::new(1, 0), Target::new(2, 0));
    ///
    /// assert!(builder.is_acyclic());
    ///
    /// builder.add_edge(Source::new(2, 0), Target::new(0, 0));
    ///
    /// assert!(!builder.is_acyclic());
    /// ```
    ///
    /// This test exists because it is possible to describe dataflow graphs that
    /// do not contain non-incrementing cycles, but without feedback nodes that
    /// strictly increment timestamps. For example,
    ///
    /// ```rust
    /// use timely::progress::frontier::Antichain;
    /// use timely::progress::{Source, Target};
    /// use timely::progress::reachability::Builder;
    ///
    /// // allocate a new empty topology builder.
    /// let mut builder = Builder::<usize>::new();
    ///
    /// // Two inputs and outputs, only one of which advances.
    /// builder.add_node(0, 2, 2, vec![
    ///     vec![Antichain::from_elem(0),Antichain::new(),],
    ///     vec![Antichain::new(),Antichain::from_elem(1),],
    /// ]);
    ///
    /// // Connect each output to the opposite input.
    /// builder.add_edge(Source::new(0, 0), Target::new(0, 1));
    /// builder.add_edge(Source::new(0, 1), Target::new(0, 0));
    ///
    /// assert!(builder.is_acyclic());
    /// ```
    pub fn is_acyclic(&self) -> bool {

        // topological sorting
        // here we treat each input / output port as a "vertex"
        // `self.edges` are edges
        // the paths in the PathSummary in `self.nodes` connecting input ports and output ports
        // are also treated as edges
        // if the PathSummary does not increase timestamp
        // i.e., default summary

        // (inputs, outputs)
        // locations: total number of ports
        let locations = self.shape.iter().map(|(targets, sources)| targets + sources).sum();
        // a hashmap that maps port (location) to in degree
        let mut in_degree = HashMap::with_capacity(locations);

        // Load edges as default summaries.
        for (index, ports) in self.edges.iter().enumerate() {
            // ports is the outgoing edges for each output node of node index.
            for (output, targets) in ports.iter().enumerate() {
                let source = Location::new_source(index, output);
                in_degree.entry(source).or_insert(0);
                for &target in targets.iter() {
                    let target = Location::from(target);
                    // .or_insert() returns a mutable reference
                    *in_degree.entry(target).or_insert(0) += 1;
                }
            }
        }

        // Load default intra-node summaries.
        for (index, summary) in self.nodes.iter().enumerate() {
            for (input, outputs) in summary.iter().enumerate() {
                let target = Location::new_target(index, input);
                in_degree.entry(target).or_insert(0);
                for (output, summaries) in outputs.iter().enumerate() {
                    // summaries is (are) the operator's internal PathSummary of path (input->output)
                    let source = Location::new_source(index, output);
                    // the summaries are incomparable
                    for summary in summaries.elements().iter() {
                        if summary == &Default::default() {
                            // if there is a default path summary
                            // it indicates there is (internal) edge input -> output
                            // that does not change timestamp
                            *in_degree.entry(source).or_insert(0) += 1;
                        }
                    }
                }
            }
        }

        // A worklist of nodes that cannot be reached from the whole graph.
        // Initially this list contains observed locations with no incoming
        // edges, but as the algorithm develops we add to it any locations
        // that can only be reached by nodes that have been on this list.

        // worklist maintains the locations (ports) with in degree == 0
        let mut worklist = Vec::with_capacity(in_degree.len());
        for (key, val) in in_degree.iter() {
            if *val == 0 {
                // Location implements Copy trait
                worklist.push(*key);
            }
        }
        in_degree.retain(|_key, val| val != &0);

        // Repeatedly remove nodes and update adjacent in-edges.
        while let Some(Location { node, port }) = worklist.pop() {
            match port {
                Port::Source(port) => {
                    // if port is an output port
                    // find outgoing edges
                    for target in self.edges[node][port].iter() {
                        let target = Location::from(*target);
                        *in_degree.get_mut(&target).unwrap() -= 1;
                        if in_degree[&target] == 0 {
                            // if in degree decreases to 0, push input port target to worklist
                            in_degree.remove(&target);
                            worklist.push(target);
                        }
                    }
                },
                Port::Target(port) => {
                    // port is an input port
                    for (output, summaries) in self.nodes[node][port].iter().enumerate() {
                        // input -> output, internal path
                        let source = Location::new_source(node, output);
                        for summary in summaries.elements().iter() {
                            if summary == &Default::default() {
                                *in_degree.get_mut(&source).unwrap() -= 1;
                                if in_degree[&source] == 0 {
                                    in_degree.remove(&source);
                                    worklist.push(source);
                                }
                            }
                        }
                    }
                },
            }
        }

        // Acyclic graphs should reduce to empty collections.
        in_degree.is_empty()
    }
}

/// An interactive tracker of propagated reachability information.
///
/// A `Tracker` tracks, for a fixed graph topology, the implications of
/// pointstamp changes at various node input and output ports. These changes may
/// alter the potential pointstamps that could arrive at downstream input ports.
pub struct Tracker<T:Timestamp> {

    /// Internal connections within hosted operators.
    ///
    /// Indexed by operator index, then input port, then output port. This is the
    /// same format returned by `get_internal_summary`, as if we simply appended
    /// all of the summaries for the hosted nodes.
    ///

    // move from builder.build()
    nodes: Vec<Vec<Vec<Antichain<T::Summary>>>>,
    /// Direct connections from sources to targets.
    ///
    /// Edges do not affect timestamps, so we only need to know the connectivity.
    /// Indexed by operator index then output port.

    // move from builder.build()
    edges: Vec<Vec<Vec<Target>>>,

    // TODO: All of the sizes of these allocations are static (except internal to `ChangeBatch`).
    //       It seems we should be able to flatten most of these so that there are a few allocations
    //       independent of the numbers of nodes and ports and such.
    //
    // TODO: We could also change the internal representation to be a graph of targets, using usize
    //       identifiers for each, so that internally we needn't use multiple levels of indirection.
    //       This may make more sense once we commit to topologically ordering the targets.

    /// Each source and target has a mutable antichain to ensure that we track their discrete frontiers,
    /// rather than their multiplicities. We separately track the frontiers resulting from propagated
    /// frontiers, to protect them from transient negativity in inbound target updates.
    per_operator: Vec<PerOperator<T>>,

    // OC: occurrence count of pointstamps
    // when the tracker is called to update the OCs
    // they are written to target_changes and source_changes as buffer
    // propagate_all() consume the buffer
    // calculate could-result-in relations

    /// Source and target changes are buffered, which allows us to delay processing until propagation,
    /// and so consolidate updates, but to leap directly to those frontiers that may have changed.
    pub(crate) target_changes: ChangeBatch<(Target, T)>,
    pub(crate) source_changes: ChangeBatch<(Source, T)>,

    /// Worklist of updates to perform, ordered by increasing timestamp and target.
    worklist: BinaryHeap<Reverse<(T, Location, i64)>>,

    /// Buffer of consequent changes.
    pushed_changes: ChangeBatch<(Location, T)>,

    /// Compiled summaries from each internal location (not scope inputs) to each scope output.
    // for each scope output (#outputs element)
    // output_changes records
    // after update OCs for the pointstamps in target_changes and source_changes
    // the pointstamps at this scope_output port with the smallest (earliest) timestamp
    // that the updates (pointstamps) we pushed could result in.

    // it records the earliest timestamp t that, some of the OCs (pointstamps) we pushed COULD-REULST-IN (t, l)
    // where l is a scope output (index the vector by l)
    // any timestamp greater than t COULD-BE-RESULT-FROM some of the OCs we pushed.
    // the ChangeBatch also returns the count of the delta of these RCs (t, l) COULD-RESULT-FROM
    output_changes: Vec<ChangeBatch<T>>,

    /// A non-negative sum of post-filtration input changes.
    ///
    /// This sum should be zero exactly when the accumulated input changes are zero,
    /// indicating that the progress tracker is currently tracking nothing. It should
    /// always be exactly equal to the sum across all operators of the frontier sizes
    /// of the target and source `pointstamps` member.
    // number of distinct pointstamps changes (pointstamps's MutableChain's frontier change)
    // instead of changes in the occurences count of pointstamps
    total_counts: i64,
}

/// Target and source information for each operator.
pub struct PerOperator<T: Timestamp> {
    /// Port information for each target.
    pub targets: Vec<PortInformation<T>>,
    /// Port information for each source.
    pub sources: Vec<PortInformation<T>>,
}

impl<T: Timestamp> PerOperator<T> {
    /// A new PerOperator bundle from numbers of input and output ports.
    pub fn new(inputs: usize, outputs: usize) -> Self {
        PerOperator {
            targets: vec![PortInformation::new(); inputs],
            sources: vec![PortInformation::new(); outputs],
        }
    }
}

/// Per-port progress-tracking information.
#[derive(Clone)]
pub struct PortInformation<T: Timestamp> {
    /// Current counts of active pointstamps.
    // this MutableAntichain maintains pointstamps count for this location (port)
    // MutableAntichain exposes the set of elements with positive count not greater than any other elements with positive count.
    // if timestamp's type has a total order
    // then pointstamps just has a single element on the FRONTIER
    // i.e., the smallest timestamp (T, i64) with positive count.
    // but after this pointstamp (time) is removed from the MutableAntichain (scheduled)
    // the MutableAntichain may put another time into its frontier via rebuild()
    // stores the occurrence counts
    pub pointstamps: MutableAntichain<T>,
    /// Current implications of active pointstamps across the dataflow.
    ///
    pub implications: MutableAntichain<T>,
    /// Path summaries to each of the scope outputs.
    pub output_summaries: Vec<Antichain<T::Summary>>,
}

impl<T: Timestamp> PortInformation<T> {
    /// Creates empty port information.
    pub fn new() -> Self {
        PortInformation {
            pointstamps: MutableAntichain::new(),
            implications: MutableAntichain::new(),
            output_summaries: Vec::new(),
        }
    }
    /// True if updates at this pointstamp uniquely block progress.
    ///
    /// This method returns true if the currently maintained pointstamp
    /// counts are such that zeroing out outstanding updates at *this*
    /// pointstamp would change the frontiers at this operator. When the
    /// method returns false it means that, temporarily at least, there
    /// are outstanding pointstamp updates that are strictly less than
    /// this pointstamp.
    // if `x.is_global(t)` returns true, it indicates the frontier of `x` will change
    // if we decrease the occurrence count of the pointstamp (x, t) by 1.
    #[inline]
    pub fn is_global(&self, time: &T) -> bool {
        // if returns true, then only a single outstanding event (pointstamp) that could-result-in (t, *this)
        let dominated = self.implications.frontier().iter().any(|t| t.less_than(time));
        // "precursor count"
        let redundant = self.implications.count_for(time) > 1;
        !dominated && !redundant
    }
}

impl<T:Timestamp> Tracker<T> {

    /// Updates the count for a time at a location.
    #[inline]
    pub fn update(&mut self, location: Location, time: T, value: i64) {
        match location.port {
            Port::Target(port) => self.update_target(Target::new(location.node, port), time, value),
            Port::Source(port) => self.update_source(Source::new(location.node, port), time, value),
        };
    }

    /// Updates the count for a time at a target (operator input, scope output).
    #[inline]
    pub fn update_target(&mut self, target: Target, time: T, value: i64) {
        // update the pointstamp occurence count
        self.target_changes.update((target, time), value);
    }
    /// Updates the count for a time at a source (operator output, scope input).
    #[inline]
    pub fn update_source(&mut self, source: Source, time: T, value: i64) {
        self.source_changes.update((source, time), value);
    }

    /// Indicates if any pointstamps have positive count.
    pub fn tracking_anything(&mut self) -> bool {
        !self.source_changes.is_empty() ||
        !self.target_changes.is_empty() ||
        self.total_counts > 0
    }

    /// Allocate a new `Tracker` using the shape from `summaries`.
    ///
    /// The result is a pair of tracker, and the summaries from each input port to each
    /// output port.
    pub fn allocate_from(builder: &Builder<T>) -> (Self, Vec<Vec<Antichain<T::Summary>>>) {

        // Allocate buffer space for each input and input port.
        // allocate for every operator
        let mut per_operator =
        builder
            .shape
            .iter()
            .map(|&(inputs, outputs)| PerOperator::new(inputs, outputs))
            .collect::<Vec<_>>();

        // Summary of scope inputs to scope outputs.
        let mut builder_summary = vec![vec![]; builder.shape[0].1];

        // Compile summaries from each location to each scope output.
        let output_summaries = summarize_outputs::<T>(&builder.nodes, &builder.edges);
        for (location, summaries) in output_summaries.into_iter() {
            // Summaries from scope inputs are useful in summarizing the scope.
            if location.node == 0 {
                if let Port::Source(port) = location.port {
                    // operator (node 0)'s Targets are the scope outputs is used f
                    // Op0 (not actually a node) -> parent inputs (Source) (child (this) scope)
                    // and its Sources are the scope inputs (index 0 is used for parent input)
                    // outputs to parents (child (this) scope) (Target) -> Op0 (not actually a node)
                    builder_summary[port] = summaries;
                }
                else {
                    // Ignore (ideally trivial) output to output summaries.
                }
            }
            // Summaries from internal nodes are important for projecting capabilities.
            else {
                match location.port {
                    Port::Target(port) => {
                        per_operator[location.node].targets[port].output_summaries = summaries;
                    },
                    Port::Source(port) => {
                        per_operator[location.node].sources[port].output_summaries = summaries;
                    },
                }
            }
        }

        // number of scope outputs
        let scope_outputs = builder.shape[0].0;
        let output_changes = vec![ChangeBatch::new(); scope_outputs];

        let tracker =
        Tracker {
            nodes: builder.nodes.clone(),
            edges: builder.edges.clone(),
            per_operator,
            target_changes: ChangeBatch::new(),
            source_changes: ChangeBatch::new(),
            worklist: BinaryHeap::new(),
            pushed_changes: ChangeBatch::new(),
            output_changes,
            total_counts: 0,
        };

        (tracker, builder_summary)
    }

    /// Propagates all pending updates.
    ///
    /// The method drains `self.input_changes` and circulates their implications
    /// until we cease deriving new implications.
    pub fn propagate_all(&mut self) {

        // Step 1: Drain `self.input_changes` and determine actual frontier changes.
        //
        // Not all changes in `self.input_changes` may alter the frontier at a location.
        // By filtering the changes through `self.pointstamps` we react only to discrete
        // changes in the frontier, rather than changes in the pointstamp counts that
        // witness that frontier.
        for ((target, time), diff) in self.target_changes.drain() {

            // get the handle to the PortInformation
            let operator = &mut self.per_operator[target.node].targets[target.port];
            // put changes into the port-local OC (occurrence counts) storage.
            let changes = operator.pointstamps.update_iter(Some((time, diff)));

            for (time, diff) in changes {
                self.total_counts += diff;
                for (output, summaries) in operator.output_summaries.iter().enumerate() {
                    // output is the index of scope output
                    // summaries is the corresponding PathSummaries from target -> scope output (one of)
                    // propagate the changes
                    let output_changes = &mut self.output_changes[output];
                    summaries
                        .elements()
                        .iter()
                        // flat_map here takes out Option wrapping
                        .flat_map(|summary| summary.results_in(&time))
                        .for_each(|out_time| output_changes.update(out_time, diff));
                }
                self.worklist.push(Reverse((time, Location::from(target), diff)));
            }
        }

        for ((source, time), diff) in self.source_changes.drain() {
            // do the same for output ports
            let operator = &mut self.per_operator[source.node].sources[source.port];
            let changes = operator.pointstamps.update_iter(Some((time, diff)));

            for (time, diff) in changes {
                self.total_counts += diff;
                for (output, summaries) in operator.output_summaries.iter().enumerate() {
                    let output_changes = &mut self.output_changes[output];
                    summaries
                        .elements()
                        .iter()
                        .flat_map(|summary| summary.results_in(&time))
                        .for_each(|out_time| output_changes.update(out_time, diff));
                }
                self.worklist.push(Reverse((time, Location::from(source), diff)));
            }
        }

        // Step 2: Circulate implications of changes to `self.pointstamps`.
        //
        // TODO: The argument that this always terminates is subtle, and should be made.
        //       The intent is that that by moving forward in layers through `time`, we
        //       will discover zero-change times when we first visit them, as no further
        //       changes can be made to them once we complete them.
        // self.worklist pulls in increasing timestamp order
        while let Some(Reverse((time, location, mut diff))) = self.worklist.pop() {

            // Drain and accumulate all updates that have the same time and location.
            while self.worklist.peek().map(|x| ((x.0).0 == time) && ((x.0).1 == location)).unwrap_or(false) {
                diff += (self.worklist.pop().unwrap().0).2;
            }

            // Only act if there is a net change, positive or negative.
            if diff != 0 {

                match location.port {
                    // Update to an operator input.
                    // Propagate any changes forward across the operator.
                    Port::Target(port_index) => {

                        // update the OCs of the port itself's pointstamp
                        // we have already did this in Step 1 for pointstamps
                        // implications record the earliest timestamp t that
                        // after this batch of OC updates we pushed to target_changes and source_changes
                        // that some of the pointstamps COULD-RESULT-IN t
                        // and also records the counts
                        // it has a frontier
                        // it also maintains some sort of "precursor count"
                        // we need some sort of cumulative sum to calculate the precise precursor counts

                        // changes to the frontier of implications
                        let changes =
                        self.per_operator[location.node]
                            .targets[port_index]
                            .implications
                            .update_iter(Some((time, diff)));

                        // propagate along the graph
                        for (time, diff) in changes {
                            let nodes = &self.nodes[location.node][port_index];
                            for (output_port, summaries) in nodes.iter().enumerate()  {
                                let source = Location { node: location.node, port: Port::Source(output_port) };
                                for summary in summaries.elements().iter() {
                                    if let Some(new_time) = summary.results_in(&time) {
                                        self.worklist.push(Reverse((new_time, source, diff)));
                                    }
                                }
                            }
                            self.pushed_changes.update((location, time), diff);
                        }
                    }
                    // Update to an operator output.
                    // Propagate any changes forward along outgoing edges.
                    Port::Source(port_index) => {

                        let changes =
                        self.per_operator[location.node]
                            .sources[port_index]
                            .implications
                            .update_iter(Some((time, diff)));

                        for (time, diff) in changes {
                            for new_target in self.edges[location.node][port_index].iter() {
                                self.worklist.push(Reverse((
                                    time.clone(),
                                    Location::from(*new_target),
                                    diff,
                                )));
                            }
                            self.pushed_changes.update((location, time), diff);
                        }
                    },
                };
            }
        }
    }

    /// Implications of maintained capabilities projected to each output.
    pub fn pushed_output(&mut self) -> &mut [ChangeBatch<T>] {
        &mut self.output_changes[..]
    }

    /// A mutable reference to the pushed results of changes.
    pub fn pushed(&mut self) -> &mut ChangeBatch<(Location, T)> {
        &mut self.pushed_changes
    }

    /// Reveals per-operator frontier state.
    pub fn node_state(&self, index: usize) -> &PerOperator<T> {
        &self.per_operator[index]
    }

    /// Indicates if pointstamp is in the scope-wide frontier.
    ///
    /// Such a pointstamp would, if removed from `self.pointstamps`, cause a change
    /// to `self.implications`, which is what we track for per operator input frontiers.
    /// If the above do not hold, then its removal either 1. shouldn't be possible,
    /// or 2. will not affect the output of `self.implications`.
    pub fn is_global(&self, location: Location, time: &T) -> bool {
        match location.port {
            Port::Target(port) => self.per_operator[location.node].targets[port].is_global(time),
            Port::Source(port) => self.per_operator[location.node].sources[port].is_global(time),
        }
    }
}

/// Determines summaries from locations to scope outputs.
///
/// Specifically, for each location whose node identifier is non-zero, we compile
/// the summaries along which they can reach each output.
///
/// Graph locations may be missing from the output, in which case they have no
/// paths to scope outputs.
// for each location, returns a vector contains PathSummaries (Antichain) of simple paths
// (that incurs least delta to timestamps) from the location to all the outputs of the scopes
// (input nodes on operator 0)
fn summarize_outputs<T: Timestamp>(
    nodes: &Vec<Vec<Vec<Antichain<T::Summary>>>>,
    edges: &Vec<Vec<Vec<Target>>>,
    ) -> HashMap<Location, Vec<Antichain<T::Summary>>>
{
    // A reverse edge map, to allow us to walk back up the dataflow graph.
    // in the original graph, an output node -> multiple input nodes
    // in reverse mapping, a input node -> an output node
    // so we can just use HashMap<Location, Location>
    // instead of HashMap<Location, Vec<Location>>
    let mut reverse = HashMap::new();
    for (node, outputs) in edges.iter().enumerate() {
        for (output, targets) in outputs.iter().enumerate() {
            for target in targets.iter() {
                reverse.insert(
                    Location::from(*target),
                    Location { node, port: Port::Source(output) }
                );
            }
        }
    }

    // results that for each location, provides a PathSummary to the scope output
    let mut results = HashMap::new();
    let mut worklist = VecDeque::<(Location, usize, T::Summary)>::new();

    // outputs are the input ports (Target) on operator (node) 0.
    // since each input port is connected only via a single edge
    // outputs do not contain repeated elements (locations)
    let outputs =
    edges
        .iter()
        // if we use map here, then flat_map's closure would receive x as Iter<Vec<Target>>
        // the map adapter is very useful, but only when the closure argument produces values.
        // if it produces an iterator instead, there’s an extra layer of indirection.
        // flat_map() will remove this extra layer on its own.
        .flat_map(|x| x.iter())
        .flat_map(|x| x.iter())
        .filter(|target| target.node == 0);

    // The scope may have no outputs, in which case we can do no work.
    // backtrack the scope (subgraph) starting from the scope outputs (they are input nodes on operator 0)
    // starting from an empty PathSummary
    for output_target in outputs {
        worklist.push_back((Location::from(*output_target), output_target.port, Default::default()));
    }

    // Loop until we stop discovering novel reachability paths.
    while let Some((location, output, summary)) = worklist.pop_front() {

        match location.port {

            // This is an output port of an operator, or a scope input.
            // We want to crawl up the operator, to its inputs.
            // backtrack output -> input (internal path)
            Port::Source(output_port) => {

                // Consider each input port of the associated operator.
                for (input_port, summaries) in nodes[location.node].iter().enumerate() {
                    // the paths (summaries) from input_port -> output_port
                    // Determine the current path summaries from the input port.
                    let location = Location { node: location.node, port: Port::Target(input_port) };
                    // PathSummary from input_port -> scope outputs
                    let antichains = results.entry(location).or_insert(Vec::new());
                    // results.entry(location)[output] is the PathSummaries from location to the scope output indexed by output
                    while antichains.len() <= output { antichains.push(Antichain::new()); }

                    // Combine each operator-internal summary to the output with `summary`.
                    // summaries[output_port] is an Antichain of PathSummaries
                    for operator_summary in summaries[output_port].elements().iter() {
                        // summary is captured in `while let` is a PathSummary from the output_port of this node (operator)
                        // to scope output indexed by output
                        // it is a PathSummary current output_port
                        // captures summary from (one of) the scope outputs -> location
                        if let Some(combined) = operator_summary.followed_by(&summary) {
                            if antichains[output].insert(combined.clone()) {
                                // further backtrace from this location (to scope output indexed by output)
                                worklist.push_back((location, output, combined));
                            }
                        }
                    }
                }

            },

            // This is an input port of an operator, or a scope output.
            // We want to walk back the edges leading to it.
            Port::Target(_port) => {

                // Each target should have (at most) one source.
                if let Some(source) = reverse.get(&location) {
                    // follow the reverse edges. they do not affect PathSummary
                    let antichains = results.entry(*source).or_insert(Vec::new());
                    while antichains.len() <= output { antichains.push(Antichain::new()); }

                    // just a small note: PathSummary only requires Clone but not Copy
                    if antichains[output].insert(summary.clone()) {
                        worklist.push_back((*source, output, summary.clone()));
                    }
                }

            },
        }

    }

    results
}
