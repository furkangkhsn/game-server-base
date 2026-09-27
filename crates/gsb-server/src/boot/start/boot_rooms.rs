//! The boot rooms (`room_count`): created ahead of every join (B44).
//!
//! The registry drains its ONE mailbox in order, and a `CreateRoom`
//! puts the room in its table before the next message is read (the
//! factory runs inside the registry loop). So a boot room exists for
//! every join exactly when its `CreateRoom` is in the mailbox before
//! any accept loop can send one: startup enqueues them here, in id
//! order, before it spawns the accept loops. Only the REPLIES are
//! awaited, from a spawned task — startup never waits on a room, so
//! nothing here can hang `start` (and with it `stop`, which needs the
//! handle `start` returns).
//!
//! The enqueue is a `try_send`: the mailbox is bounded, and a boot room
//! past its free capacity (thousands of boot rooms) would need an
//! awaited send — startup waiting on the registry. Those rooms are sent
//! from the spawned task instead, still in id order after the inline
//! ones, but no longer ahead of the first joins: the one remaining case
//! of the old race, named in the warning.

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::config::RoomTemplate;
use gsb_core::channel::Mailbox;
use gsb_core::error::CoreError;
use gsb_core::registry::{RegistryMsg, RoomStatus};

#[cfg(test)]
mod tests;

type Reply = oneshot::Receiver<Result<RoomStatus, CoreError>>;

/// Enqueue `CreateRoom` for rooms `1..=count` into `registry` NOW (see
/// the module docs), and spawn the task that logs their replies — and
/// sends the rooms the mailbox had no room for.
pub(super) fn create_boot_rooms(
    registry: &Mailbox<RegistryMsg>,
    template: &RoomTemplate,
    count: u64,
) {
    let mut replies: Vec<Reply> = Vec::new();
    let mut overflow = None;
    for id in 1..=count {
        let (reply, rx) = oneshot::channel();
        let config = template.room(id);
        match registry.try_send(RegistryMsg::CreateRoom { config, reply }) {
            Ok(()) => replies.push(rx),
            Err(TrySendError::Full(msg)) => {
                warn!(
                    first = id,
                    last = count,
                    "boot rooms past the registry mailbox's capacity are created \
                     asynchronously: a join to them right at startup may find no room"
                );
                overflow = Some((msg, rx, id));
                break;
            }
            Err(TrySendError::Closed(_)) => {
                warn!("registry gone before the boot rooms");
                return;
            }
        }
    }
    let late = overflow.map(|(msg, rx, id)| (registry.clone(), template.clone(), msg, rx, id));
    tokio::spawn(async move {
        if let Some((registry, template, first, rx, id)) = late {
            send_rest(&registry, &template, first, rx, id, count, &mut replies).await;
        }
        for rx in replies {
            log_reply(rx).await;
        }
    });
}

/// The boot rooms the mailbox had no room for — `first` (room `id`)
/// and the ones after it — sent in id order, each send waiting for
/// capacity (this is the spawned task, never startup).
async fn send_rest(
    registry: &Mailbox<RegistryMsg>,
    template: &RoomTemplate,
    first: RegistryMsg,
    rx: Reply,
    id: u64,
    count: u64,
    replies: &mut Vec<Reply>,
) {
    if registry.send(first).await.is_err() {
        return;
    }
    replies.push(rx);
    for id in id + 1..=count {
        let (reply, rx) = oneshot::channel();
        let config = template.room(id);
        if registry
            .send(RegistryMsg::CreateRoom { config, reply })
            .await
            .is_err()
        {
            return;
        }
        replies.push(rx);
    }
}

/// Log one boot room's creation reply (as startup always has).
async fn log_reply(rx: Reply) {
    match rx.await {
        Ok(Ok(status)) => info!(?status, "room created"),
        Ok(Err(e)) => warn!(error = %e, "room creation failed"),
        Err(_) => warn!("registry gone before room reply"),
    }
}
