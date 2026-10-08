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
/// gsb event `fixture.good.GoldReceived` (op 2001, to WORLD).
impl ::gsb_protocol::event::Event for GoldReceived {
    const OP: u16 = 2001;
    const TO: ::gsb_protocol::event::Dest = ::gsb_protocol::event::Dest::World;
}
impl ::gsb_protocol::event::WorldEvent for GoldReceived {}
/// gsb event `fixture.good.ChatLine` (op 2002, to CLIENT).
impl ::gsb_protocol::event::Event for ChatLine {
    const OP: u16 = 2002;
    const TO: ::gsb_protocol::event::Dest = ::gsb_protocol::event::Dest::Client;
}
impl ::gsb_protocol::event::ClientEvent for ChatLine {}
/// gsb event `fixture.good.BalanceChanged` (op 2005, to BOTH).
impl ::gsb_protocol::event::Event for BalanceChanged {
    const OP: u16 = 2005;
    const TO: ::gsb_protocol::event::Dest = ::gsb_protocol::event::Dest::Both;
}
impl ::gsb_protocol::event::WorldEvent for BalanceChanged {}
impl ::gsb_protocol::event::ClientEvent for BalanceChanged {}
/// gsb event `fixture.good.Presence.Online` (op 2006, to CLIENT).
impl ::gsb_protocol::event::Event for presence::Online {
    const OP: u16 = 2006;
    const TO: ::gsb_protocol::event::Dest = ::gsb_protocol::event::Dest::Client;
}
impl ::gsb_protocol::event::ClientEvent for presence::Online {}
