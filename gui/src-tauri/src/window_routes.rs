//! Route ownership must outlive a closing webview's JavaScript. A route can
//! be used by several windows; only its last window releases it to the node.
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(crate) struct WindowRoutes {
    owners: HashMap<String, HashSet<(String, u64)>>,
    generations: HashMap<String, u64>,
}

impl WindowRoutes {
    pub(crate) fn acquire(&mut self, window: &str, route: &str) -> u64 {
        let generation = *self.generations.entry(window.into()).or_default();
        self.owners
            .entry(route.into())
            .or_default()
            .insert((window.into(), generation));
        generation
    }

    pub(crate) fn is_current(&self, window: &str, generation: u64) -> bool {
        self.generations.get(window).copied().unwrap_or_default() == generation
    }

    /// Explicit disconnects for untracked routes keep their existing meaning.
    pub(crate) fn release(&mut self, window: &str, route: &str) -> bool {
        let generation = self.generations.get(window).copied().unwrap_or_default();
        self.release_generation(window, generation, route)
    }

    pub(crate) fn release_generation(
        &mut self,
        window: &str,
        generation: u64,
        route: &str,
    ) -> bool {
        if let Some(owners) = self.owners.get_mut(route) {
            owners.remove(&(window.into(), generation));
            if !owners.is_empty() {
                return false;
            }
            self.owners.remove(route);
        }
        true
    }

    pub(crate) fn close(&mut self, window: &str) -> Vec<String> {
        let generation = *self.generations.entry(window.into()).or_default();
        self.generations
            .insert(window.into(), generation.saturating_add(1));
        let routes = self
            .owners
            .iter()
            .filter(|(_, owners)| owners.contains(&(window.into(), generation)))
            .map(|(route, _)| route.clone())
            .collect::<Vec<_>>();
        routes
            .into_iter()
            .filter(|route| self.release_generation(window, generation, route))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_console_releases_all_its_legs_even_without_javascript_cleanup() {
        let mut routes = WindowRoutes::default();
        for route in ["screen", "audio", "mouse", "clipboard"] {
            routes.acquire("console-laptop", route);
            routes.acquire("console-laptop", route); // repeated snapshot/reconnect
        }
        let mut closed = routes.close("console-laptop");
        closed.sort();
        assert_eq!(closed, ["audio", "clipboard", "mouse", "screen"]);
        assert!(routes.close("console-laptop").is_empty());
    }

    #[test]
    fn another_windows_route_survives_until_its_last_owner_closes() {
        let mut routes = WindowRoutes::default();
        routes.acquire("console-laptop", "screen");
        routes.acquire("video-popout", "screen");
        routes.acquire("room", "room-audio");
        assert!(!routes.release("console-laptop", "screen"));
        assert!(routes.close("console-laptop").is_empty());
        assert_eq!(routes.close("video-popout"), ["screen"]);
        assert_eq!(routes.close("room"), ["room-audio"]);
        assert!(routes.release("main", "untracked-graph-route"));
    }

    #[test]
    fn late_connect_after_close_cannot_release_a_reopened_consoles_route() {
        let mut routes = WindowRoutes::default();
        let previous = routes.acquire("console", "screen");
        assert_eq!(routes.close("console"), ["screen"]);
        let next = routes.acquire("console", "screen");
        assert!(!routes.is_current("console", previous));
        assert!(routes.is_current("console", next));
        assert!(!routes.release_generation("console", previous, "screen"));
        assert_eq!(routes.close("console"), ["screen"]);
        assert!(routes.release_generation("console", previous, "old-camera"));
    }
}
