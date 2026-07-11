//! The capability-caster generator shared by the `interface` root (whole
//! feature areas cast from `Messenger`) and its submodules (per-operation
//! capabilities cast from a feature trait).

/// Generates "caster" traits exposing optional capabilities `$cap` of a
/// `$base` implementor, plus the specialization pair that resolves — at
/// compile time, per concrete backend type — whether the capability exists.
/// Accepts one or more declarations per invocation, back to back.
///
/// The blanket impl answers [`MessengerError::NotImplemented`]; backends that
/// implement `$cap` get the specialized impl returning `Ok(self)`. Making the
/// caster a supertrait of `$base` puts it in `$base`'s vtable, so the check is
/// a plain synchronous call through `dyn $base` — no probing, no network.
///
/// Expansion-site requirements: `MessengerError`, `Arc`, `$base`, and `$cap`
/// must be in scope.
///
/// # Example
///
/// ```ignore
/// capability_caster!(
///     /// Capability: voice chat integration.
///     MessengerCasterVoice: Messenger => Voice { voice, arc_voice }
/// );
/// ```
///
/// expands to (one such block per declaration):
///
/// ```ignore
/// /// Capability: voice chat integration.
/// pub trait MessengerCasterVoice {
///     fn voice(&self) -> Result<&dyn Voice, MessengerError>;
///     fn arc_voice(self: Arc<Self>) -> Result<Arc<dyn Voice>, MessengerError>;
/// }
/// // Base case: any `Messenger` answers NotImplemented...
/// impl<T: Messenger> MessengerCasterVoice for T {
///     default fn voice(&self) -> Result<&dyn Voice, MessengerError> {
///         Err(MessengerError::NotImplemented)
///     }
///     default fn arc_voice(self: Arc<Self>) -> Result<Arc<dyn Voice>, MessengerError> {
///         Err(MessengerError::NotImplemented)
///     }
/// }
/// // ...overridden (via `#![feature(specialization)]`) for backends whose
/// // concrete type also implements `Voice`, which is what makes the check
/// // compile-time per backend.
/// impl<T: Messenger + Voice + 'static> MessengerCasterVoice for T {
///     fn voice(&self) -> Result<&dyn Voice, MessengerError> {
///         Ok(self)
///     }
///     fn arc_voice(self: Arc<Self>) -> Result<Arc<dyn Voice>, MessengerError> {
///         Ok(self)
///     }
/// }
/// ```
///
/// [`MessengerError::NotImplemented`]: super::MessengerError::NotImplemented
macro_rules! capability_caster {
    ($(
        $(#[$attr:meta])*
        $caster:ident: $base:ident => $cap:ident { $method:ident, $arc_method:ident }
    )+) => {$(
        $(#[$attr])*
        pub trait $caster {
            fn $method(&self) -> Result<&dyn $cap, MessengerError>;
            fn $arc_method(self: Arc<Self>) -> Result<Arc<dyn $cap>, MessengerError>;
        }
        impl<T: $base> $caster for T {
            default fn $method(&self) -> Result<&dyn $cap, MessengerError> {
                Err(MessengerError::NotImplemented)
            }
            default fn $arc_method(self: Arc<Self>) -> Result<Arc<dyn $cap>, MessengerError> {
                Err(MessengerError::NotImplemented)
            }
        }
        impl<T: $base + $cap + 'static> $caster for T {
            fn $method(&self) -> Result<&dyn $cap, MessengerError> {
                Ok(self)
            }
            fn $arc_method(self: Arc<Self>) -> Result<Arc<dyn $cap>, MessengerError> {
                Ok(self)
            }
        }
    )+};
}
pub(crate) use capability_caster;
