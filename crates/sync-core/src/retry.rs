use openpush_domain::SendState;
pub const fn may_retry_transport(state: SendState) -> bool {
    state.can_retry_transport()
}
pub const fn may_retry_carrier(state: SendState) -> bool {
    state.can_retry_carrier()
}
