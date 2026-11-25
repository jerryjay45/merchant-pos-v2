//! Business logic for Merchant POS. No UI, no database driver in here:
//! everything the UI needs goes through the types and traits in this crate.

pub mod auth;
pub mod case_pricing;
pub mod catalog;
pub mod discount;
pub mod money;
pub mod nav;
pub mod product;
pub mod product_edit;
pub mod repo;
pub mod setup;
pub mod user;

pub use auth::{check_session_gate, new_password_hash, verify_password, AuthError, AuthService};
pub use repo::{Business, BusinessInfo, ConfigRepository, SessionRepository, StorageError, UserRepository};
pub use setup::{needs_setup, run_setup, SetupData, SetupError};
pub use money::{format_money, to_cents};
pub use nav::{areas_for, Area, Page};
pub use product::{Membership, Pager, ProductPage, ProductQuery, ProductRepository, ProductRow};
pub use user::{Role, User, UserRecord};
pub use product_edit::{
    parse_cost, price_from_markup, price_hint, warnings_for_edit, Group, GroupKind, GroupSync, PriceGroup, ProductDetail,
    ProductDraft, ProductError, ProductWarning, ProductWrite, UpdateOutcome,
};
pub use discount::{
    apply_discount, fmt_percent, parse_date, AppliedDiscount, DiscountLevel, DiscountSetting, Rule, SlotSetting, SlotSource,
};
pub use case_pricing::{follows_unit, price_case, savings_cents, CaseLink, CaseSetting, PriceMode, Unit};
pub use catalog::{
    clean_group_name, markup_text, parse_level, parse_markup_percent, CatalogRepository, LevelInput, LevelRow, ListPage, ListQuery, MarkupChange,
    MarkupGroupRow, MarkupOutcome,
};
