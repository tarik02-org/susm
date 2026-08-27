#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub struct Transition<State, Effect> {
    pub state: State,
    pub effects: Vec<Effect>,
}

impl<State, Effect> Transition<State, Effect> {
    pub const fn new(state: State, effects: Vec<Effect>) -> Self {
        Self { state, effects }
    }

    pub fn without_effects(state: State) -> Self {
        Self {
            state,
            effects: Vec::new(),
        }
    }
}
