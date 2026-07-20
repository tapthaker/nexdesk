use crate::ports::ServerChannel;

/// Session behavior when one post-handshake server channel ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerChannelDisposition {
    ContinueWithoutChannel,
    Disconnect,
}

pub fn server_channel_disposition(channel: ServerChannel) -> ServerChannelDisposition {
    match channel {
        ServerChannel::Clipboard => ServerChannelDisposition::ContinueWithoutChannel,
        ServerChannel::Control | ServerChannel::Input => ServerChannelDisposition::Disconnect,
    }
}

#[cfg(test)]
mod tests {
    use crate::ports::{ServerPeerLink, ServerTransportEvent, TransportFailure};
    use crate::testing::ScriptedServerPeerLink;

    use super::*;

    #[tokio::test]
    async fn closure_and_failure_scenarios_cover_every_server_channel() {
        let cases = [
            (
                ServerTransportEvent::Closed(ServerChannel::Control),
                ServerChannelDisposition::Disconnect,
            ),
            (
                ServerTransportEvent::Failed(TransportFailure::new(
                    ServerChannel::Control,
                    "control failed",
                )),
                ServerChannelDisposition::Disconnect,
            ),
            (
                ServerTransportEvent::Closed(ServerChannel::Input),
                ServerChannelDisposition::Disconnect,
            ),
            (
                ServerTransportEvent::Failed(TransportFailure::new(
                    ServerChannel::Input,
                    "input failed",
                )),
                ServerChannelDisposition::Disconnect,
            ),
            (
                ServerTransportEvent::Closed(ServerChannel::Clipboard),
                ServerChannelDisposition::ContinueWithoutChannel,
            ),
            (
                ServerTransportEvent::Failed(TransportFailure::new(
                    ServerChannel::Clipboard,
                    "clipboard failed",
                )),
                ServerChannelDisposition::ContinueWithoutChannel,
            ),
        ];

        for (event, expected) in cases {
            let peer = ScriptedServerPeerLink::new();
            peer.push_event(event.clone());
            let received = peer.next_event().await.unwrap();

            assert_eq!(received, event);
            assert_eq!(server_channel_disposition(received.channel()), expected);
            assert_eq!(peer.pending_events(), 0);
            peer.shutdown().await;
        }
    }
}
