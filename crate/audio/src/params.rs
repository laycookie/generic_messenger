/// The derive that generates [`AfxParams`] impls, re-exported so the trait
/// and its derive share one import path.
pub use simple_audio_channels_macro::AfxParams;

/// How a parameter's value should be displayed by a UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ParamUnit {
    /// A linear amplitude multiplier; typically shown as a percentage.
    Amplitude,
    /// Decibels.
    Decibels,
    /// A duration in milliseconds.
    Milliseconds,
    /// A frequency in hertz.
    Hertz,
}

/// Opaque handle to one parameter of one effect. Values are generated into an
/// effect's static `AFX_PARAMS` table and index that same table; the derive
/// also emits one named constant per parameter (the field name uppercased,
/// e.g. `Gain::DESIRED_MULTIPLIER`) for direct use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamKey(pub(crate) usize);

/// UI description of one tunable effect parameter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamSpec {
    pub key: ParamKey,
    /// Display label.
    pub name: &'static str,
    pub unit: ParamUnit,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    /// The range minimum stands for "off" (-infinity) rather than its literal
    /// value.
    pub min_is_off: bool,
}

/// One parameter in a snapshot: its spec and current value.
#[derive(Clone, Copy, Debug)]
pub struct ParamState {
    pub spec: &'static ParamSpec,
    pub value: f32,
}

/// Generated parameter surface for an effect.
///
/// Effects usually derive this with `#[derive(AfxParams)]` and field
/// annotations such as `#[afx(param(...))]`. Paramless effects derive it with
/// no annotated fields, producing an empty parameter list.
pub trait AfxParams {
    fn param_specs(&self) -> &'static [ParamSpec];
    fn param_value(&self, key: ParamKey) -> Option<f32>;
    fn set_param(&mut self, key: ParamKey, value: f32) -> bool;
}
