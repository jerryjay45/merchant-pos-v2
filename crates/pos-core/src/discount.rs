//! Discounts on a product: up to two tiers, each either a shared named level (5 % off 3+) or a
//! custom rule typed on the product (35 % off 3+), optionally limited to a date range, and either
//! overriding or stacking. Pure functions, no UI and no database, so the dialog, the cashier and
//! the tests all use the same rules.
//!
//! How a sale picks the discount (ported from `_apply_discount` in the Python cashier, with the
//! date range and the explicit stack option added):
//!  * a tier only counts while today is inside its dates (no dates = always) and the quantity
//!    reaches its minimum;
//!  * if both tiers count, tier 2 replaces tier 1 ("override", the Python behaviour), or the two
//!    percentages are added when `stack` is on;
//!  * the discount per unit is rounded to the cent, like the Python app.

/// Highest minimum quantity the form accepts (like the Python spin box).
pub const MAX_MIN_QTY: u32 = 9999;
/// Custom percentages run from 0.1 to 99.9.
pub const MIN_PERCENT: f64 = 0.1;
pub const MAX_PERCENT: f64 = 99.9;

/// A shared named level (`discount_levels`): "LEVEL 1", 5 %, 3 or more.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscountLevel {
    pub id: i64,
    pub name: String,
    pub min_qty: u32,
    /// In percent: 5.0 means 5 %.
    pub percent: f64,
}

impl DiscountLevel {
    /// "LEVEL 1  (5% off, min qty 3)", as in the Python product dialog.
    pub fn label(&self) -> String {
        format!("{}  ({}% off, min qty {})", self.name, fmt_percent(self.percent), self.min_qty)
    }
}

/// Where a tier's rule comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum SlotSource {
    Level(i64),
    Custom { min_qty: u32, percent: f64 },
}

/// One tier as stored on the product.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotSetting {
    pub source: SlotSource,
    /// First and last day (`YYYY-MM-DD`, inclusive) the tier counts; `None` = no limit.
    pub starts: Option<String>,
    pub ends: Option<String>,
}

/// What a product stores about its discounts. The default is "no discounts".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiscountSetting {
    pub slot1: Option<SlotSetting>,
    pub slot2: Option<SlotSetting>,
    /// Tier 2 adds to tier 1 when both count (otherwise it replaces it).
    pub stack: bool,
}

/// A tier with its numbers filled in.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// 1 or 2.
    pub slot: u8,
    pub min_qty: u32,
    pub percent: f64,
    pub starts: Option<String>,
    pub ends: Option<String>,
}

impl Rule {
    /// Is `today` (`YYYY-MM-DD`) inside the tier's dates?
    pub fn active_on(&self, today: &str) -> bool {
        self.starts.as_deref().map_or(true, |s| s <= today) && self.ends.as_deref().map_or(true, |e| today <= e)
    }

    pub fn applies(&self, qty: u32, today: &str) -> bool {
        qty >= self.min_qty && self.active_on(today)
    }
}

/// The discount a sale line gets.
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedDiscount {
    pub percent: f64,
    /// Taken off each unit, in cents.
    pub per_unit_cents: i64,
    /// Which tiers were used (`[2]`, or `[1, 2]` when stacked).
    pub slots: Vec<u8>,
}

impl DiscountSetting {
    /// Checks what the form collected; the message is shown next to the dialog.
    pub fn validate(&self) -> Result<(), String> {
        for (n, slot) in [(1, &self.slot1), (2, &self.slot2)] {
            let Some(s) = slot else { continue };
            if let SlotSource::Custom { min_qty, percent } = &s.source {
                if !(1..=MAX_MIN_QTY).contains(min_qty) {
                    return Err(format!("Discount {n}: the minimum quantity must be between 1 and {MAX_MIN_QTY}."));
                }
                if !percent.is_finite() || !(MIN_PERCENT..=MAX_PERCENT).contains(percent) {
                    return Err(format!("Discount {n}: the percentage must be between {MIN_PERCENT} and {MAX_PERCENT}."));
                }
            }
            for d in [&s.starts, &s.ends].into_iter().flatten() {
                parse_date(d).map_err(|e| format!("Discount {n}: {e}"))?;
            }
            if let (Some(a), Some(b)) = (&s.starts, &s.ends) {
                if a > b {
                    return Err(format!("Discount {n}: the end date is before the start date."));
                }
            }
        }
        Ok(())
    }

    /// The rules of both tiers. A tier that names a level that no longer exists is left out.
    pub fn rules(&self, levels: &[DiscountLevel]) -> Vec<Rule> {
        let mut out = Vec::new();
        for (n, slot) in [(1u8, &self.slot1), (2u8, &self.slot2)] {
            let Some(s) = slot else { continue };
            let (min_qty, percent) = match &s.source {
                SlotSource::Custom { min_qty, percent } => (*min_qty, *percent),
                SlotSource::Level(id) => match levels.iter().find(|l| l.id == *id) {
                    Some(l) => (l.min_qty, l.percent),
                    None => continue,
                },
            };
            out.push(Rule { slot: n, min_qty, percent, starts: s.starts.clone(), ends: s.ends.clone() });
        }
        out
    }

    /// One plain-language line per tier for the dialog, e.g.
    /// `Discount 2: 35% off at 3 or more, 2026-12-01 to 2026-12-24 (not active today)`.
    pub fn summary(&self, levels: &[DiscountLevel], today: &str) -> Vec<String> {
        let mut lines: Vec<String> = self
            .rules(levels)
            .iter()
            .map(|r| {
                let window = match (&r.starts, &r.ends) {
                    (Some(a), Some(b)) => format!(", {a} to {b}"),
                    (Some(a), None) => format!(", from {a}"),
                    (None, Some(b)) => format!(", until {b}"),
                    (None, None) => String::new(),
                };
                let status = if r.active_on(today) { "" } else { " (not active today)" };
                let when = if r.min_qty <= 1 { "any quantity".to_string() } else { format!("{} or more", r.min_qty) };
                format!("Discount {}: {}% off at {when}{window}{status}", r.slot, fmt_percent(r.percent))
            })
            .collect();
        if self.stack && lines.len() == 2 {
            lines.push("Both discounts add up when they apply together.".to_string());
        }
        lines
    }
}

/// The discount for `qty` units at `unit_price_cents` on `today` (`YYYY-MM-DD`), if any.
pub fn apply_discount(rules: &[Rule], stack: bool, qty: u32, unit_price_cents: i64, today: &str) -> Option<AppliedDiscount> {
    let counting: Vec<&Rule> = rules.iter().filter(|r| r.applies(qty, today)).collect();
    if counting.is_empty() {
        return None;
    }
    let (percent, slots) = if stack {
        let sum: f64 = counting.iter().map(|r| r.percent).sum();
        (sum.min(100.0), counting.iter().map(|r| r.slot).collect())
    } else {
        // Tier 2 replaces tier 1, even if its percentage is lower (as the Python cashier does).
        let top = counting.iter().max_by_key(|r| r.slot).expect("not empty");
        (top.percent, vec![top.slot])
    };
    let per_unit_cents = ((unit_price_cents as f64) * percent / 100.0).round() as i64;
    Some(AppliedDiscount { percent, per_unit_cents: per_unit_cents.clamp(0, unit_price_cents.max(0)), slots })
}

/// `YYYY-MM-DD` -> the same text if it is a real date; empty -> `None`.
pub fn parse_date(text: &str) -> Result<Option<String>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let bad = || "dates look like 2026-12-24 (year-month-day).".to_string();
    let parts: Vec<&str> = t.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return Err(bad());
    }
    if !parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
        return Err(bad());
    }
    let (y, m, d): (u32, u32, u32) = (parts[0].parse().unwrap(), parts[1].parse().unwrap(), parts[2].parse().unwrap());
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + leap as u32,
        _ => return Err(format!("{t} is not a real date.")),
    };
    if d == 0 || d > days {
        return Err(format!("{t} is not a real date."));
    }
    Ok(Some(t.to_string()))
}

/// 5.0 -> "5", 2.5 -> "2.5", 33.333 -> "33.33".
pub fn fmt_percent(p: f64) -> String {
    let s = format!("{p:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TODAY: &str = "2026-12-10";

    fn rule(slot: u8, min_qty: u32, percent: f64, starts: Option<&str>, ends: Option<&str>) -> Rule {
        Rule { slot, min_qty, percent, starts: starts.map(Into::into), ends: ends.map(Into::into) }
    }

    fn slot(min_qty: u32, percent: f64, starts: Option<&str>, ends: Option<&str>) -> Option<SlotSetting> {
        Some(SlotSetting { source: SlotSource::Custom { min_qty, percent }, starts: starts.map(Into::into), ends: ends.map(Into::into) })
    }

    #[test]
    fn nothing_below_the_minimum_quantity() {
        let r = [rule(1, 3, 5.0, None, None)];
        assert_eq!(apply_discount(&r, false, 2, 1000, TODAY), None);
        let d = apply_discount(&r, false, 3, 1000, TODAY).unwrap();
        assert_eq!((d.percent, d.per_unit_cents, d.slots), (5.0, 50, vec![1]));
    }

    #[test]
    fn tier_two_replaces_tier_one_even_when_lower_like_the_python_cashier() {
        let r = [rule(1, 3, 10.0, None, None), rule(2, 6, 5.0, None, None)];
        assert_eq!(apply_discount(&r, false, 4, 1000, TODAY).unwrap().percent, 10.0);
        let d = apply_discount(&r, false, 6, 1000, TODAY).unwrap();
        assert_eq!((d.percent, d.slots), (5.0, vec![2]));
    }

    #[test]
    fn the_promo_scenario_35_percent_for_3_instead_of_the_usual_5() {
        // usual: 5 % off 1+, promo: 35 % off 3+ from Dec 1 to Dec 24
        let r = [rule(1, 1, 5.0, None, None), rule(2, 3, 35.0, Some("2026-12-01"), Some("2026-12-24"))];
        let at = |qty, day| apply_discount(&r, false, qty, 10_000, day).map(|d| (d.percent, d.per_unit_cents));
        assert_eq!(at(1, "2026-12-10"), Some((5.0, 500)));
        assert_eq!(at(2, "2026-12-10"), Some((5.0, 500)));
        assert_eq!(at(3, "2026-12-10"), Some((35.0, 3500)), "promo wins while it runs");
        assert_eq!(at(3, "2026-12-01"), Some((35.0, 3500)), "first day counts");
        assert_eq!(at(3, "2026-12-24"), Some((35.0, 3500)), "last day counts");
        assert_eq!(at(3, "2026-11-30"), Some((5.0, 500)), "before it starts: the usual discount");
        assert_eq!(at(3, "2026-12-25"), Some((5.0, 500)), "after it ends: the usual discount");
    }

    #[test]
    fn stacking_adds_the_percentages_and_is_capped() {
        let r = [rule(1, 1, 5.0, None, None), rule(2, 3, 10.0, None, None)];
        let d = apply_discount(&r, true, 3, 2000, TODAY).unwrap();
        assert_eq!((d.percent, d.per_unit_cents, d.slots), (15.0, 300, vec![1, 2]));
        assert_eq!(apply_discount(&r, true, 2, 2000, TODAY).unwrap().slots, vec![1], "only tier 1 reaches qty 2");
        let big = [rule(1, 1, 60.0, None, None), rule(2, 1, 70.0, None, None)];
        let d = apply_discount(&big, true, 1, 1000, TODAY).unwrap();
        assert_eq!((d.percent, d.per_unit_cents), (100.0, 1000));
    }

    #[test]
    fn rounds_to_the_cent_per_unit() {
        let r = [rule(1, 1, 33.0, None, None)];
        assert_eq!(apply_discount(&r, false, 1, 999, TODAY).unwrap().per_unit_cents, 330); // 329.67
        let r = [rule(1, 1, 2.5, None, None)];
        assert_eq!(apply_discount(&r, false, 1, 1999, TODAY).unwrap().per_unit_cents, 50); // 49.975
        assert_eq!(apply_discount(&r, false, 1, 0, TODAY).unwrap().per_unit_cents, 0);
    }

    #[test]
    fn open_ended_windows() {
        let from = rule(1, 1, 10.0, Some("2026-12-15"), None);
        assert!(!from.active_on("2026-12-14") && from.active_on("2026-12-15") && from.active_on("2030-01-01"));
        let until = rule(1, 1, 10.0, None, Some("2026-12-15"));
        assert!(until.active_on("2020-01-01") && until.active_on("2026-12-15") && !until.active_on("2026-12-16"));
    }

    #[test]
    fn levels_are_looked_up_and_missing_ones_skipped() {
        let levels = [DiscountLevel { id: 7, name: "LEVEL 1".into(), min_qty: 3, percent: 5.0 }];
        let s = DiscountSetting {
            slot1: Some(SlotSetting { source: SlotSource::Level(7), starts: None, ends: None }),
            slot2: Some(SlotSetting { source: SlotSource::Level(99), starts: None, ends: None }),
            stack: false,
        };
        assert_eq!(s.rules(&levels), vec![rule(1, 3, 5.0, None, None)]);
        assert_eq!(levels[0].label(), "LEVEL 1  (5% off, min qty 3)");
    }

    #[test]
    fn validation() {
        let ok = DiscountSetting { slot1: slot(3, 35.0, Some("2026-12-01"), Some("2026-12-24")), slot2: None, stack: false };
        assert_eq!(ok.validate(), Ok(()));
        let err = |s: DiscountSetting| s.validate().unwrap_err();
        let one = |slot: Option<SlotSetting>| DiscountSetting { slot1: slot, ..Default::default() };
        assert!(err(one(slot(0, 5.0, None, None))).contains("minimum quantity"));
        assert!(err(one(slot(10_000, 5.0, None, None))).contains("minimum quantity"));
        assert!(err(one(slot(3, 0.0, None, None))).contains("percentage"));
        assert!(err(one(slot(3, 100.0, None, None))).contains("percentage"));
        assert!(err(one(slot(3, f64::NAN, None, None))).contains("percentage"));
        assert!(err(one(slot(3, 5.0, Some("2026-12-24"), Some("2026-12-01")))).contains("end date is before"));
        assert!(err(one(slot(3, 5.0, Some("12/01/2026"), None))).contains("Discount 1"));
        let two = DiscountSetting { slot2: slot(0, 5.0, None, None), ..Default::default() };
        assert!(err(two).starts_with("Discount 2"));
        assert_eq!(DiscountSetting::default().validate(), Ok(()));
    }

    #[test]
    fn dates() {
        assert_eq!(parse_date(""), Ok(None));
        assert_eq!(parse_date(" 2026-12-24 "), Ok(Some("2026-12-24".into())));
        assert_eq!(parse_date("2028-02-29"), Ok(Some("2028-02-29".into())), "leap year");
        for bad in ["2027-02-29", "2026-13-01", "2026-00-10", "2026-04-31", "2026-4-1", "26-12-24", "2026/12/24", "abcd-ef-gh", "2026-12-24x"] {
            assert!(parse_date(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_date("1900-02-29").is_err(), true, "1900 was not a leap year");
        assert!(parse_date("2000-02-29").is_ok());
    }

    #[test]
    fn plain_language_summary() {
        let s = DiscountSetting {
            slot1: slot(1, 5.0, None, None),
            slot2: slot(3, 35.0, Some("2026-12-01"), Some("2026-12-24")),
            stack: false,
        };
        assert_eq!(
            s.summary(&[], "2026-12-10"),
            ["Discount 1: 5% off at any quantity", "Discount 2: 35% off at 3 or more, 2026-12-01 to 2026-12-24"]
        );
        assert_eq!(s.summary(&[], "2027-01-02")[1], "Discount 2: 35% off at 3 or more, 2026-12-01 to 2026-12-24 (not active today)");
        let stacked = DiscountSetting { stack: true, ..s };
        assert_eq!(stacked.summary(&[], "2026-12-10").last().unwrap(), "Both discounts add up when they apply together.");
        assert_eq!(fmt_percent(2.5), "2.5");
        assert_eq!(fmt_percent(33.333), "33.33");
        assert_eq!(fmt_percent(10.0), "10");
    }
}
