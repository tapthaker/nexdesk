use crate::ports::ClientChannel;

/// Session behavior when one post-handshake logical channel ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientChannelDisposition {
    ContinueWithoutChannel,
    Disconnect,
}

pub fn client_channel_disposition(channel: ClientChannel) -> ClientChannelDisposition {
    match channel {
        ClientChannel::Clipboard => ClientChannelDisposition::ContinueWithoutChannel,
        ClientChannel::Control | ClientChannel::Input => ClientChannelDisposition::Disconnect,
    }
}

#[cfg(test)]
mod tests {
    use crate::ports::{ClientPeerLink, ClientTransportEvent, TransportFailure};
    use crate::testing::ScriptedPeerLink;

    use super::*;

    #[tokio::test]
    async fn closure_and_failure_scenarios_cover_every_logical_channel() {
        let cases = [
            (
                ClientTransportEvent::Closed(ClientChannel::Control),
                ClientChannelDisposition::Disconnect,
            ),
            (
                ClientTransportEvent::Failed(TransportFailure::new(
                    ClientChannel::Control,
                    "control failed",
                )),
                ClientChannelDisposition::Disconnect,
            ),
            (
                ClientTransportEvent::Closed(ClientChannel::Input),
                ClientChannelDisposition::Disconnect,
            ),
            (
                ClientTransportEvent::Failed(TransportFailure::new(
                    ClientChannel::Input,
                    "input failed",
                )),
                ClientChannelDisposition::Disconnect,
            ),
            (
                ClientTransportEvent::Closed(ClientChannel::Clipboard),
                ClientChannelDisposition::ContinueWithoutChannel,
            ),
            (
                ClientTransportEvent::Failed(TransportFailure::new(
                    ClientChannel::Clipboard,
                    "clipboard failed",
                )),
                ClientChannelDisposition::ContinueWithoutChannel,
            ),
        ];

        for (event, expected) in cases {
            let peer = ScriptedPeerLink::new();
            peer.push_event(event.clone());
            let received = peer.next_event().await.unwrap();

            assert_eq!(received, event);
            assert_eq!(client_channel_disposition(received.channel()), expected);
            assert_eq!(peer.pending_events(), 0);
        }
    }
}
