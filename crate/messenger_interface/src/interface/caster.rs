//! Specialization-backed capability projections.

/// Generates borrowed and optional retained casters. Blanket implementations
/// return `NotImplemented`; capability implementations return `self` through
/// the base trait's vtable. Expansion scope must contain error, base, and
/// capability types.
macro_rules! capability_caster {
    ($(
        $(#[$attr:meta])*
        $caster:ident: $base:ident => $cap:ident {
            $method:ident $(, $retained_method:ident)?
        }
    )+) => {$(
        $(#[$attr])*
        pub trait $caster {
            /// Borrows supported capability without probing.
            fn $method(&self) -> Result<&dyn $cap, MessengerError>;

            $(
                /// Projects retained guard through concrete receiver vtable.
                fn $retained_method(
                    self: $crate::owner::Retained<Self>,
                ) -> Result<$crate::owner::Retained<dyn $cap>, MessengerError>;
            )?
        }
        impl<T: $base> $caster for T {
            default fn $method(&self) -> Result<&dyn $cap, MessengerError> {
                Err(MessengerError::NotImplemented)
            }

            $(
                default fn $retained_method(
                    self: $crate::owner::Retained<Self>,
                ) -> Result<$crate::owner::Retained<dyn $cap>, MessengerError> {
                    Err(MessengerError::NotImplemented)
                }
            )?
        }
        impl<T: $base + $cap + 'static> $caster for T {
            fn $method(&self) -> Result<&dyn $cap, MessengerError> {
                Ok(self)
            }

            $(
                fn $retained_method(
                    self: $crate::owner::Retained<Self>,
                ) -> Result<$crate::owner::Retained<dyn $cap>, MessengerError> {
                    Ok(self)
                }
            )?
        }
    )+};
}
pub(crate) use capability_caster;
