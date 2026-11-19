//! What the supervisor and manager windows show, grouped into areas (top tabs) and pages
//! (sub-tabs), and who may open each one. One list, so moving a page or sharing it with
//! supervisors is a one-line change. Mirrors `ui/shared/tab_registry.py` in the Python app
//! (same keys, same order, same roles), so both apps behave alike while the rewrite catches up.
//!
//! Access is fixed by role; there are no per-user switches.

use crate::user::Role;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// Stable id, the same as the Python registry (`products`, `stock`, `cashdrawer` ...).
    pub key: &'static str,
    pub title: &'static str,
    pub min_role: Role,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Area {
    pub key: &'static str,
    pub title: &'static str,
    pub pages: Vec<Page>,
}

const fn page(key: &'static str, title: &'static str, min_role: Role) -> Page {
    Page { key, title, min_role }
}

const SALES: &[Page] = &[
    page("reports", "Reports", Role::Supervisor),
    page("transactions", "Transactions", Role::Supervisor),
    page("void", "Void / Refund", Role::Supervisor),
];
const INVENTORY: &[Page] = &[
    page("products", "Products", Role::Supervisor),
    page("stock", "Stock", Role::Supervisor),
    page("pricetags", "Price Tags", Role::Supervisor),
    page("quickkeys", "Quick Keys", Role::Supervisor),
    page("groups", "Groups & Discounts", Role::Manager),
    page("stocksettings", "Stock Settings", Role::Manager),
];
const PRINTING: &[Page] = &[
    page("printersetup", "Printer Setup", Role::Supervisor),
    page("receipt", "Receipt (advanced)", Role::Manager),
    page("pickup", "Pickup (advanced)", Role::Manager),
];
const DATA: &[Page] = &[
    page("backup", "Backup", Role::Supervisor),
    page("import", "Import", Role::Manager),
    page("postgres", "PostgreSQL", Role::Manager),
];
const SETUP: &[Page] = &[
    page("users", "Users", Role::Manager),
    page("permissions", "Permissions", Role::Manager),
    page("business", "Business", Role::Manager),
    page("tax", "Tax & Currency", Role::Manager),
    page("checkout", "Checkout & Appearance", Role::Manager),
    page("cashdrawer", "Cash Drawer", Role::Manager),
];

const AREAS: &[(&str, &str, &[Page])] = &[
    ("sales", "Sales", SALES),
    ("inventory", "Inventory", INVENTORY),
    ("printing", "Printing", PRINTING),
    ("data", "Data", DATA),
    ("setup", "Setup", SETUP),
];

/// The areas `role` sees, each with only the pages it may open. Areas left empty are dropped.
pub fn areas_for(role: Role) -> Vec<Area> {
    AREAS
        .iter()
        .filter_map(|(key, title, pages)| {
            let pages: Vec<Page> = pages.iter().copied().filter(|p| role.can_open(p.min_role)).collect();
            (!pages.is_empty()).then(|| Area { key, title, pages })
        })
        .collect()
}

/// Pages that already exist in this Rust version. The window shows a "coming next" note for the rest.
pub const BUILT_PAGES: &[&str] = &["products"];

pub fn is_built(key: &str) -> bool {
    BUILT_PAGES.contains(&key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(role: Role) -> Vec<&'static str> {
        areas_for(role).iter().flat_map(|a| a.pages.iter().map(|p| p.key)).collect()
    }

    #[test]
    fn supervisor_sees_the_simple_pages_only() {
        let k = keys(Role::Supervisor);
        for want in ["reports", "products", "stock", "printersetup", "backup"] {
            assert!(k.contains(&want), "supervisor should see {want}");
        }
        for hidden in ["groups", "stocksettings", "receipt", "import", "users", "cashdrawer", "tax"] {
            assert!(!k.contains(&hidden), "supervisor must not see {hidden}");
        }
        // Setup is manager-only, so the whole area disappears for a supervisor.
        assert!(areas_for(Role::Supervisor).iter().all(|a| a.key != "setup"));
    }

    #[test]
    fn manager_sees_everything_in_python_order() {
        let areas = areas_for(Role::Manager);
        let titles: Vec<_> = areas.iter().map(|a| a.title).collect();
        assert_eq!(titles, ["Sales", "Inventory", "Printing", "Data", "Setup"]);
        let inv: Vec<_> = areas[1].pages.iter().map(|p| p.key).collect();
        assert_eq!(inv, ["products", "stock", "pricetags", "quickkeys", "groups", "stocksettings"]);
    }

    #[test]
    fn cashiers_get_no_dashboard() {
        assert!(areas_for(Role::Cashier).is_empty());
    }

    #[test]
    fn keys_are_unique() {
        let mut k = keys(Role::Manager);
        let n = k.len();
        k.sort();
        k.dedup();
        assert_eq!(k.len(), n);
    }

    #[test]
    fn role_ranks() {
        assert!(Role::Manager.can_open(Role::Supervisor));
        assert!(Role::Supervisor.can_open(Role::Supervisor));
        assert!(!Role::Supervisor.can_open(Role::Manager));
        assert!(!Role::Cashier.can_open(Role::Supervisor));
    }
}
