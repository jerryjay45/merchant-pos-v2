//! Case / pack pricing: what a case of N singles costs and sells for. Pure functions, no UI and
//! no database, so the dialog preview, the save and the price cascades all give the same answer.
//!
//! A case item is "N units of a single product" (or of a variant group). Its **cost** is always
//! the single's cost x N. Its **selling price** follows one of three modes:
//!
//!  * `Markup`: cost + the shop's case profit % (default 10 %). This is what the Python app does
//!    for every case, so existing data is in this mode.
//!  * `PackDiscount`: N x the single's selling price, less a pack discount % ("buy the case, get
//!    5 % off the singles' total"). It follows the single's price.
//!  * `Manual`: a price typed for this case. Cost still follows the single, the price never moves
//!    by itself.
//!
//! Whenever the single's cost or price changes, linked cases in `Markup` and `PackDiscount` mode
//! are repriced; `Manual` cases only get their cost.

/// Highest units-per-case the form accepts (like the Python spin box).
pub const MAX_UNITS: u32 = 9999;
pub const MIN_PACK_DISCOUNT: f64 = 0.1;
pub const MAX_PACK_DISCOUNT: f64 = 99.9;

/// What a case is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseLink {
    /// A single product: its cost and price are the unit values.
    Product(i64),
    /// A variant group (a case that can be any flavour): the group's cost and price are the unit values.
    VariantGroup(i64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PriceMode {
    Markup,
    /// Percent off the singles' total, 0.1 to 99.9.
    PackDiscount { percent: f64 },
    Manual,
}

/// What a case item stores about being a case.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseSetting {
    /// `None`: not linked to anything, the cost is typed and the price comes from the markup group.
    pub link: Option<CaseLink>,
    pub units: u32,
    pub mode: PriceMode,
}

impl CaseSetting {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_UNITS).contains(&self.units) {
            return Err(format!("Units per case must be between 1 and {MAX_UNITS}."));
        }
        if let PriceMode::PackDiscount { percent } = self.mode {
            if !percent.is_finite() || !(MIN_PACK_DISCOUNT..=MAX_PACK_DISCOUNT).contains(&percent) {
                return Err(format!("The pack discount must be between {MIN_PACK_DISCOUNT} and {MAX_PACK_DISCOUNT} %."));
            }
            if self.link.is_none() {
                return Err("A pack discount needs a single product (or variant group) to take the price from.".to_string());
            }
        }
        Ok(())
    }
}

/// The cost and selling price of the single (or variant group) a case is made of, in cents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unit {
    pub cost_cents: i64,
    pub price_cents: i64,
}

fn round_cents(v: f64) -> i64 {
    v.round() as i64
}

/// The cost and price a case gets. `typed_*` are what the form (or the stored case) holds now and
/// are kept wherever the single gives nothing to derive from. `profit_pct` is the shop's case
/// profit as a fraction (0.10 = 10 %).
pub fn price_case(typed_cost: i64, typed_price: i64, unit: Unit, units: u32, mode: PriceMode, profit_pct: f64) -> (i64, i64) {
    let units = i64::from(units.max(1));
    let cost = if unit.cost_cents > 0 { unit.cost_cents * units } else { typed_cost };
    let price = match mode {
        PriceMode::Manual => typed_price,
        PriceMode::Markup => round_cents(cost as f64 * (1.0 + profit_pct)),
        PriceMode::PackDiscount { percent } => {
            if unit.price_cents > 0 {
                round_cents((unit.price_cents * units) as f64 * (1.0 - percent / 100.0))
            } else {
                typed_price
            }
        }
    };
    (cost, price)
}

/// Does a change to the single reach this case at all? (`Manual` still takes the cost.)
pub fn follows_unit(unit: Unit, mode: PriceMode) -> bool {
    unit.cost_cents > 0 || (matches!(mode, PriceMode::PackDiscount { .. }) && unit.price_cents > 0)
}

/// What buying the case saves against N singles, in cents (negative = the case costs more).
pub fn savings_cents(unit_price_cents: i64, units: u32, case_price_cents: i64) -> i64 {
    unit_price_cents * i64::from(units) - case_price_cents
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIT: Unit = Unit { cost_cents: 100, price_cents: 180 };

    #[test]
    fn markup_mode_is_the_python_formula() {
        // 12 x $1.00 = $12.00 cost, +10 % = $13.20
        assert_eq!(price_case(0, 0, UNIT, 12, PriceMode::Markup, 0.10), (1200, 1320));
        assert_eq!(price_case(0, 0, UNIT, 24, PriceMode::Markup, 0.10), (2400, 2640));
        assert_eq!(price_case(0, 0, UNIT, 1, PriceMode::Markup, 0.0), (100, 100));
    }

    #[test]
    fn pack_discount_comes_off_the_singles_total() {
        // 12 x $1.80 = $21.60, 10 % off = $19.44; cost is still 12 x $1.00
        assert_eq!(price_case(0, 0, UNIT, 12, PriceMode::PackDiscount { percent: 10.0 }, 0.10), (1200, 1944));
        assert_eq!(savings_cents(180, 12, 1944), 216);
        // rounds to the cent: 6 x $0.99 = 594, 12.5 % off = 519.75 -> 520
        let u = Unit { cost_cents: 50, price_cents: 99 };
        assert_eq!(price_case(0, 0, u, 6, PriceMode::PackDiscount { percent: 12.5 }, 0.10).1, 520);
    }

    #[test]
    fn manual_keeps_the_typed_price_but_cost_follows_the_single() {
        assert_eq!(price_case(0, 1500, UNIT, 12, PriceMode::Manual, 0.10), (1200, 1500));
    }

    #[test]
    fn without_a_priced_single_the_typed_numbers_stay() {
        let none = Unit { cost_cents: 0, price_cents: 0 };
        // markup works off the typed cost, like the Python form
        assert_eq!(price_case(2000, 0, none, 12, PriceMode::Markup, 0.10), (2000, 2200));
        assert_eq!(price_case(2000, 2500, none, 12, PriceMode::Manual, 0.10), (2000, 2500));
        assert_eq!(price_case(2000, 2500, none, 12, PriceMode::PackDiscount { percent: 5.0 }, 0.10), (2000, 2500));
        // a single with a price but no cost still drives a pack-discount case
        let price_only = Unit { cost_cents: 0, price_cents: 200 };
        assert_eq!(price_case(2000, 0, price_only, 10, PriceMode::PackDiscount { percent: 10.0 }, 0.10), (2000, 1800));
    }

    #[test]
    fn what_follows_the_single() {
        let none = Unit { cost_cents: 0, price_cents: 0 };
        let price_only = Unit { cost_cents: 0, price_cents: 200 };
        assert!(follows_unit(UNIT, PriceMode::Markup));
        assert!(!follows_unit(none, PriceMode::Markup));
        assert!(!follows_unit(price_only, PriceMode::Markup));
        assert!(follows_unit(price_only, PriceMode::PackDiscount { percent: 5.0 }));
    }

    #[test]
    fn a_pack_discount_can_make_the_case_cheaper_than_its_cost_so_the_caller_can_warn() {
        // 12 x $1.80 less 50 % = $10.80 against a $12.00 cost
        let (cost, price) = price_case(0, 0, UNIT, 12, PriceMode::PackDiscount { percent: 50.0 }, 0.10);
        assert!(price < cost);
    }

    #[test]
    fn validation() {
        let ok = CaseSetting { link: Some(CaseLink::Product(1)), units: 12, mode: PriceMode::PackDiscount { percent: 10.0 } };
        assert_eq!(ok.validate(), Ok(()));
        let with = |units, mode, link| CaseSetting { link, units, mode };
        let p = Some(CaseLink::Product(1));
        assert!(with(0, PriceMode::Markup, p).validate().unwrap_err().contains("Units per case"));
        assert!(with(10_000, PriceMode::Markup, p).validate().is_err());
        assert!(with(6, PriceMode::PackDiscount { percent: 0.0 }, p).validate().unwrap_err().contains("pack discount"));
        assert!(with(6, PriceMode::PackDiscount { percent: 100.0 }, p).validate().is_err());
        assert!(with(6, PriceMode::PackDiscount { percent: f64::NAN }, p).validate().is_err());
        assert!(with(6, PriceMode::PackDiscount { percent: 5.0 }, None).validate().unwrap_err().contains("needs a single product"));
        assert_eq!(with(6, PriceMode::Markup, None).validate(), Ok(()));
        assert_eq!(with(6, PriceMode::Manual, Some(CaseLink::VariantGroup(2))).validate(), Ok(()));
    }
}
