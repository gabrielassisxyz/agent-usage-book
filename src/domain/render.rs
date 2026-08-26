//! Render-context ports.
//!
//! A bare scalar must not be able to escape into a user interface: the free-standing
//! `Display` ban enforced elsewhere in this bead is the negative half of that rule, and
//! this module is the positive half's *interface*, not its implementation. It declares
//! what a rendering helper needs to turn a domain quantity into text; it implements no
//! rendering itself. `aub-xus.2` owns the actual presentation helpers that consume this
//! port.

/// How many fractional digits to render a quantity with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Precision(u8);

impl Precision {
    /// Constructs from a raw digit count.
    pub const fn new(digits: u8) -> Self {
        Self(digits)
    }

    /// The number of fractional digits.
    pub const fn digits(self) -> u8 {
        self.0
    }
}

/// The explicit context a rendering helper needs to turn a bare quantity into text.
///
/// A unit label and a precision policy are the two pieces every quantity needs; a value
/// that can be partial or estimated additionally carries its own coverage and evidence
/// quality (`aub-rif.8`), which a renderer reads from the value itself rather than from
/// this context. No type in this crate implements `RenderContext` yet: it exists so
/// `aub-xus.2`'s helpers have a contract to take as an explicit parameter, instead of
/// each renderer growing its own ad hoc idea of how a number becomes text.
pub trait RenderContext {
    /// The unit label to render alongside the value, e.g. `"credits"` or `"%"`.
    fn unit_label(&self) -> &str;

    /// The precision to render the value at.
    fn precision(&self) -> Precision;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precision_round_trips_its_digit_count() {
        assert_eq!(Precision::new(2).digits(), 2);
        assert_eq!(Precision::new(0).digits(), 0);
    }

    /// A minimal implementer, so the trait is proven to be implementable at all and not
    /// merely declared. This is test-only scaffolding, not a positive rendering helper:
    /// it renders nothing, it only supplies the two pieces of context the trait defines.
    struct FixedContext {
        label: &'static str,
        precision: Precision,
    }

    impl RenderContext for FixedContext {
        fn unit_label(&self) -> &str {
            self.label
        }

        fn precision(&self) -> Precision {
            self.precision
        }
    }

    #[test]
    fn a_context_reports_its_unit_label_and_precision() {
        let ctx = FixedContext {
            label: "credits",
            precision: Precision::new(2),
        };
        assert_eq!(ctx.unit_label(), "credits");
        assert_eq!(ctx.precision().digits(), 2);
    }
}
