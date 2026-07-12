use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GraphError<T> {
    Duplicate(T),
    Missing { node: T, dependency: T },
    Cycle,
}

/// Deterministic dependency traversal shared by spoke scheduling and artifact
/// invalidation. Edges point from a node to the prerequisites it waits for.
pub(crate) struct DependencyGraph<T> {
    dependencies: BTreeMap<T, BTreeSet<T>>,
    dependents: BTreeMap<T, BTreeSet<T>>,
}

impl<T> DependencyGraph<T>
where
    T: Clone + Ord,
{
    pub(crate) fn new(
        entries: impl IntoIterator<Item = (T, impl IntoIterator<Item = T>)>,
    ) -> Result<Self, GraphError<T>> {
        let mut dependencies = BTreeMap::<T, BTreeSet<T>>::new();
        for (node, deps) in entries {
            if dependencies
                .insert(node.clone(), deps.into_iter().collect())
                .is_some()
            {
                return Err(GraphError::Duplicate(node));
            }
        }
        let mut dependents = dependencies
            .keys()
            .cloned()
            .map(|node| (node, BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        for (node, deps) in &dependencies {
            for dependency in deps {
                let Some(children) = dependents.get_mut(dependency) else {
                    return Err(GraphError::Missing {
                        node: node.clone(),
                        dependency: dependency.clone(),
                    });
                };
                children.insert(node.clone());
            }
        }
        Ok(Self {
            dependencies,
            dependents,
        })
    }

    pub(crate) fn topological_waves(&self) -> Result<Vec<Vec<T>>, GraphError<T>> {
        let mut remaining = self.dependencies.clone();
        let mut waves = Vec::new();
        while !remaining.is_empty() {
            let ready = remaining
                .iter()
                .filter(|(_, dependencies)| dependencies.is_empty())
                .map(|(node, _)| node.clone())
                .collect::<Vec<_>>();
            if ready.is_empty() {
                return Err(GraphError::Cycle);
            }
            for node in &ready {
                remaining.remove(node);
            }
            for dependencies in remaining.values_mut() {
                for node in &ready {
                    dependencies.remove(node);
                }
            }
            waves.push(ready);
        }
        Ok(waves)
    }

    pub(crate) fn transitive_dependents(&self, roots: impl IntoIterator<Item = T>) -> BTreeSet<T> {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from_iter(roots);
        while let Some(node) = queue.pop_front() {
            if let Some(children) = self.dependents.get(&node) {
                for child in children {
                    if seen.insert(child.clone()) {
                        queue.push_back(child.clone());
                    }
                }
            }
        }
        seen
    }
}

#[cfg(test)]
mod tests {
    use super::{DependencyGraph, GraphError};

    #[test]
    fn partitions_true_topological_waves() {
        let graph = DependencyGraph::new([
            ("a", Vec::<&str>::new()),
            ("b", vec![]),
            ("c", vec!["a", "b"]),
        ])
        .unwrap();
        assert_eq!(
            graph.topological_waves().unwrap(),
            vec![vec!["a", "b"], vec!["c"]]
        );
    }

    #[test]
    fn rejects_cycles_before_dispatch() {
        let graph = DependencyGraph::new([("a", vec!["b"]), ("b", vec!["a"])]).unwrap();
        assert_eq!(graph.topological_waves(), Err(GraphError::Cycle));
    }

    #[test]
    fn diamond_dependents_are_deduplicated() {
        let graph = DependencyGraph::new([
            ("a", Vec::<&str>::new()),
            ("c", vec!["a"]),
            ("d", vec!["a"]),
            ("e", vec!["c", "d"]),
            ("unrelated", vec![]),
        ])
        .unwrap();
        assert_eq!(
            graph
                .transitive_dependents(["a"])
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["c", "d", "e"]
        );
    }
}
