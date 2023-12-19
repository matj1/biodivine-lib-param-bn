use crate::_impl_regulatory_graph::signed_directed_graph::Sign::{Negative, Positive};
use crate::{RegulatoryGraph, VariableId};
use std::collections::{HashMap, HashSet};
use std::ops::Add;
use biodivine_lib_bdd::{Bdd, BddPartialValuation, BddValuation, BddVariable, BddVariableSet};
use z3::{FuncDecl, SatResult, Solver, Sort};
use z3::ast::Bool;

/// **(internal)** Basic utility methods for manipulating the `SdGraph`.
mod _impl_sd_graph;

/// **(internal)** Compute basic reachability properties within the `SdGraph`.
mod _reachability;

/// **(internal)** Perform a decomposition of the signed directed graph (or its subgraph)
/// into strongly connected components.
mod _strongly_connected_components;

/// **(internal)** Perform decomposition of the signed directed graph (or its subgraph)
/// into weakly connected components.
mod _weakly_connected_components;

/// **(internal)** Algorithms for detecting smallest cycles, including positive/negative
/// variants for each algorithms.
mod _cycle_detection;

/// **(internal)** Algorithm for computing an approximation of the minimum feedback vertex set.
mod _feedback_vertex_set;

/// **(internal)** Algorithm for computing an approximation of the maximum independent cycles set.
mod _independent_cycles;

/// A sign enum that describes the monotonicity of edges.
///
/// TODO: If we rewrite the API at some point, this should merge with `Monotonicity`.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum Sign {
    Positive,
    Negative,
}

/// A more efficient representation of a signed directed graph that can be used for studying
/// the properties of a `RegulatoryGraph`.
///
/// TODO: If we rewrite the API at some point, this should just merge with a `RegulatoryGraph`.
#[derive(Clone, Debug)]
pub struct SdGraph {
    successors: Vec<Vec<(VariableId, Sign)>>,
    predecessors: Vec<Vec<(VariableId, Sign)>>,
}

impl Add for Sign {
    type Output = Sign;

    fn add(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (Positive, Positive) => Positive,
            (Negative, Negative) => Positive,
            (Positive, Negative) => Negative,
            (Negative, Positive) => Negative,
        }
    }
}

/// Algorithms for analysing the underlying signed directed graph.
impl RegulatoryGraph {
    /// Compute all *non-trivial* strongly connected components of the regulatory graph.
    ///
    /// The result is sorted by component size.
    pub fn strongly_connected_components(&self) -> Vec<HashSet<VariableId>> {
        SdGraph::from(self).strongly_connected_components()
    }

    /// Compute all *non-trivial* strongly connected components restricted within the given set
    /// of vertices.
    ///
    /// The result is sorted by component size.
    pub fn restricted_strongly_connected_components(
        &self,
        restriction: &HashSet<VariableId>,
    ) -> Vec<HashSet<VariableId>> {
        SdGraph::from(self).restricted_strongly_connected_components(restriction)
    }

    /// Compute all variables that transitively regulate the given `target` variable.
    pub fn transitive_regulators(&self, target: VariableId) -> HashSet<VariableId> {
        SdGraph::from(self).backward_reachable(HashSet::from([target]))
    }

    /// Compute all variables that are transitively regulated by the given `regulator` variable.
    pub fn transitive_targets(&self, regulator: VariableId) -> HashSet<VariableId> {
        SdGraph::from(self).forward_reachable(HashSet::from([regulator]))
    }

    /// Compute the shortest cycle that contains the given `pivot` vertex, or `None` if there
    /// is no such cycle.
    pub fn shortest_cycle(&self, pivot: VariableId) -> Option<Vec<VariableId>> {
        let graph = SdGraph::from(self);
        graph.shortest_cycle(&graph.mk_all_vertices(), pivot, usize::MAX)
    }

    /// Compute the shortest simple cycle that contains the given `pivot` vertex and has the
    /// specified `target_parity`.
    pub fn shortest_parity_cycle(
        &self,
        pivot: VariableId,
        target_parity: Sign,
    ) -> Option<Vec<VariableId>> {
        let graph = SdGraph::from(self);
        graph.shortest_parity_cycle(&graph.mk_all_vertices(), pivot, target_parity, usize::MAX)
    }

    /// Compute the set of variables that, if removed, cause this `RegulatoryGraph` to become
    /// acyclic.
    ///
    /// The method tries to obtain a minimal such set, but the minimality is not guaranteed.
    pub fn feedback_vertex_set(&self) -> HashSet<VariableId> {
        let graph = SdGraph::from(self);
        graph.restricted_feedback_vertex_set(&graph.mk_all_vertices())
    }

    pub fn exact_fvs_solver(&self) -> HashSet<VariableId> {
        let z3 = z3::Context::new(&z3::Config::new());
        let bool_sort = Sort::bool(&z3);

        let variable_constructors = self.variables()
            .map(|it| {
                let name = self.get_variable_name(it);
                FuncDecl::new(&z3, name.as_str(), &[], &bool_sort)
            })
            .collect::<Vec<_>>();

        let variable_constants = variable_constructors
            .iter()
            .map(|it| it.apply(&[]).as_bool().unwrap())
            .collect::<Vec<_>>();

        let cycles = self.independent_cycles();
        if cycles.is_empty() {
            return HashSet::new();
        }

        let var_counts = variable_constants.iter()
            .map(|it| (it, 1))
            .collect::<Vec<_>>();

        let graph = SdGraph::from(self);

        let mut target_size = cycles.len() as i32;
        let solver = Solver::new(&z3);
        loop {
            println!("Start searching for size {}.", target_size);
            let constraint = Bool::pb_eq(&z3, &var_counts, target_size);
            solver.push();
            solver.assert(&constraint);
            match solver.check() {
                SatResult::Unknown => unreachable!("This must be decidable."),
                SatResult::Unsat => {
                    // There is no FVS of the target size. We have to increase the limit.
                    println!("Nothing found. Increasing target size to {}.", target_size);
                    solver.pop(1);
                    target_size += 1;
                    continue
                },
                SatResult::Sat => {
                    // We have a candidate.
                    let model = solver.get_model().unwrap();
                    println!("Found FVS candidate.");

                    // Translate model into a candidate FVS.
                    let mut fvs_candidate = HashSet::new();
                    for (i, term) in variable_constants.iter().enumerate() {
                        let is_valid = model.eval(term, true).unwrap().as_bool().unwrap();
                        if is_valid {
                            fvs_candidate.insert(VariableId(i));
                        }
                    }

                    // Search for conflict cycles.
                    let restriction = self.variables()
                        .filter(|it| !fvs_candidate.contains(it))
                        .collect::<HashSet<_>>();
                    let mut conflict_cycles = Vec::new();
                    for var in self.variables().rev() {
                        if !restriction.contains(&var) {
                            continue;
                        }
                        if let Some(cycle) = graph.shortest_cycle(&restriction, var, usize::MAX) {
                            conflict_cycles.push(cycle);
                        }
                    }
                    if conflict_cycles.is_empty() {
                        println!("FVS is correct and minimal.");
                        return fvs_candidate;
                    }
                    println!("Found {} conflict cycles.", conflict_cycles.len());
                    conflict_cycles.sort_by(|a, b| a.len().cmp(&b.len()));
                    // Assert the shortest cycle.
                    let cycle_members = conflict_cycles[0].iter()
                        .map(|var| &variable_constants[var.to_index()])
                        .collect::<Vec<_>>();
                    let assertion = Bool::or(&z3, &cycle_members);

                    println!("Adding assertion of size {}.", cycle_members.len());
                    solver.pop(1);  // remove the old count assertion
                    solver.assert(&assertion);
                }
            }
        }
    }

    pub fn exact_fvs(&self) -> HashSet<VariableId> {
        println!("{:?}", self.strongly_connected_components().iter().map(|it| it.len()).collect::<Vec<_>>());
        let mut candidate_variables = Vec::new();
        for var in self.variables() {
            if self.shortest_cycle(var).is_none() {
                // Acyclic variable.
                continue;
            }
            let regulators = self.regulators(var);
            let targets = self.targets(var);
            if regulators.len() == 1 && candidate_variables.contains(&regulators[0]) {
                // Skip if we have just one regulator and it is already included.
                continue;
            }
            if targets.len() == 1 && candidate_variables.contains(&targets[0]) {
                continue;
            }
            candidate_variables.push(var);
        }

        println!("There are {} variables, and {} are relevant.", self.num_vars(), candidate_variables.len());

        let ctx = BddVariableSet::new_anonymous(u16::try_from(candidate_variables.len()).unwrap());
        let all_vars = ctx.variables();
        let bdd_vars = ctx.variables();
        let bdd_vars = bdd_vars.into_iter().zip(candidate_variables)
            .map(|(a, b)| (b, a))
            .collect::<HashMap<_, _>>();

        let cycles = self.independent_cycles();
        if cycles.is_empty() {
            return HashSet::new();
        }
        // Upper bound will be updated as we go to better results.
        let upper_bound = self.feedback_vertex_set();
        println!("Upper bound: {}", upper_bound.len());

        let min_size = cycles.len();
        let max_size = upper_bound.len();
        if min_size == max_size {
            // Sometimes we are lucky.
            return upper_bound;
        }
        // true = variable in FVS; false = variable not in FVS
        // Initial candidates are #greedy_cycles <= candidate < #greedy_fvs. We already have a witness for #greedy_fvs,
        // so we don't need to include it in the search.
        //let mut candidates = ctx.mk_sat_up_to_k(max_size - 1, &bdd_vars);
        //candidates = candidates.and_not(&ctx.mk_sat_up_to_k(min_size - 1, &bdd_vars));

        fn build_cycle_clause(
            ctx: &BddVariableSet,
            bdd_vars: &HashMap<VariableId, BddVariable>,
            cycle: &[VariableId]
        ) -> Bdd {
            let mut valuation = BddPartialValuation::empty();
            for var in cycle {
                if let Some(var) = bdd_vars.get(var) {
                    valuation[*var] = Some(true);
                }
            }
            ctx.mk_disjunctive_clause(&valuation)
        }

        fn valuation_to_fvs(
            bdd_vars: &HashMap<VariableId, BddVariable>,
            valuation: &BddValuation,
        ) -> HashSet<VariableId> {
            let mut result = HashSet::new();
            for (n_var, b_var) in bdd_vars.iter() {
                if valuation[*b_var] {
                    result.insert(*n_var);
                }
            }
            result
        }

        let graph = SdGraph::from(self);
        for k in min_size..max_size {
        //for k in (min_size..max_size).rev() {
            println!("Start with {}", k);

            let mut candidates = ctx.mk_sat_exactly_k(k, &all_vars);
            //let mut candidates = ctx.mk_sat_up_to_k(max_size - 1, &bdd_vars);
            //candidates = candidates.and_not(&ctx.mk_sat_up_to_k(min_size - 1, &bdd_vars));

            // Each cycles is a new potential clause that needs to be satisfied:
            //let mut cycles = cycles.clone();
            //cycles.sort_by(|a, b| a.len().cmp(&b.len()));
            //cycles.reverse();
            //candidates = candidates.and(&build_cycle_clause(&ctx, &bdd_vars, &cycles[0]));
            //println!("Apply cycle: {}; Candidates: {}", cycles[0].len(), candidates.size());
            /*for cycle in &cycles {
                candidates = candidates.and(&build_cycle_clause(&ctx, &bdd_vars, &cycle));
                println!("Apply cycle: {}; Candidates: {}", cycle.len(), candidates.size());
            }*/

            while let Some(candidate) = candidates.most_negative_valuation() {
                let fvs = valuation_to_fvs(&bdd_vars, &candidate);
                let restriction = self.variables()
                    .filter(|it| !fvs.contains(it))
                    .collect::<HashSet<_>>();
                let mut conflict_cycles = Vec::new();
                for var in self.variables().rev() {
                    if !restriction.contains(&var) {
                        continue;
                    }
                    if let Some(cycle) = graph.shortest_cycle(&restriction, var, usize::MAX) {
                        conflict_cycles.push(cycle);
                    }
                }
                if conflict_cycles.is_empty() {
                    return fvs;
                }
                println!("Found {} conflict cycles", conflict_cycles.len());
                //conflict_cycles.sort_by(|a, b| a.len().cmp(&b.len()));

                // This should put the cycle with the "largest" variable first.
                conflict_cycles.sort_by(|a, b| {
                    a.iter().max().unwrap().cmp(&b.iter().max().unwrap()).reverse()
                });

                //conflict_cycles.reverse();
                /*for cycle in conflict_cycles {
                    // Found counterexample. Assert that at least one of the vertices on that cycle must
                    // be a member of the FVS.
                    candidates = candidates.and(&build_cycle_clause(&ctx, &bdd_vars, &cycle));
                    println!("Counterexample {}. Candidate: {}", cycle.len(), candidates.size());
                }*/
                candidates = candidates.and(&build_cycle_clause(&ctx, &bdd_vars, &conflict_cycles[0]));
                println!("Apply cycle: {}; Candidates: {}", conflict_cycles[0].len(), candidates.size());
                /*
                // This is an FVS that is smaller than the current upper bound. We can thus eliminate
                // all FVSes of this size or larger.
                assert!(fvs.len() < upper_bound.len());
                upper_bound = fvs;
                candidates = candidates.and(&ctx.mk_sat_up_to_k(upper_bound.len() - 1, &bdd_vars));
                println!("Decreased size to {}. Candidate: {}", upper_bound.len(), candidates.size());*/
            }
        }

        upper_bound
    }

    /// Compute the set of variables that, if removed, causes this `RegulatoryGraph` to lose
    /// all cycles of the specified parity.
    ///
    /// The method tries to obtain a minimal such set, but the minimality is not guaranteed.
    pub fn parity_feedback_vertex_set(&self, parity: Sign) -> HashSet<VariableId> {
        let graph = SdGraph::from(self);
        graph.restricted_parity_feedback_vertex_set(&graph.mk_all_vertices(), parity)
    }

    /// Compute a collection of independent cycles of this `RegulatoryGraph`. That is, disjoint
    /// cycles that intersect every cycle in the graph.
    ///
    /// The method tries to obtain a maximal such set, but the maximality is not guaranteed.
    pub fn independent_cycles(&self) -> Vec<Vec<VariableId>> {
        let graph = SdGraph::from(self);
        graph.restricted_independent_cycles(&graph.mk_all_vertices())
    }

    /// Compute a collection of independent cycles of this `RegulatoryGraph` with desired `parity`.
    /// That is, disjoint cycles of the given `parity` that intersect every other cycle of this
    /// `parity` in the graph.
    ///
    /// The method tries to obtain a maximal such set, but the maximality is not guaranteed.
    pub fn independent_parity_cycles(&self, parity: Sign) -> Vec<Vec<VariableId>> {
        let graph = SdGraph::from(self);
        graph.restricted_independent_parity_cycles(&graph.mk_all_vertices(), parity)
    }
}
