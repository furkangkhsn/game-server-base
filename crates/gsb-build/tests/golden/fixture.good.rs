/// gsb request `fixture.good.Game.Transfer` (op 1001): answered with `TransferDone`, or the typed `GameError`.
impl ::gsb_protocol::request::Call for TransferGold {
    type Response = TransferDone;
    type Error = GameError;
}
impl ::gsb_protocol::request::Request for TransferGold {
    const OP: u16 = 1001;
}
/// gsb request `fixture.good.Workshop.Make` (op 1003): answered with `Crafted`, or the typed `super::common::SharedError`.
impl ::gsb_protocol::request::Call for Craft {
    type Response = Crafted;
    type Error = super::common::SharedError;
}
impl ::gsb_protocol::request::Request for Craft {
    const OP: u16 = 1003;
}
