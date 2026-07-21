use std::time::Duration;

use nexdesk::ports::{DiscoveryEvent, PeerDiscovery};
use nexdesk::{start_advertising, MdnsDiscovery};

#[tokio::test]
#[ignore = "requires a host environment with multicast mDNS enabled"]
async fn real_mdns_adapter_discovers_local_advertisement() {
    const PORT: u16 = 42_420;
    let _advertisement = start_advertising(PORT).expect("start real mDNS advertisement");
    let mut browse = MdnsDiscovery
        .browse()
        .await
        .expect("start real mDNS browse");

    let peer = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match browse.next_event().await.expect("read mDNS event") {
                Some(DiscoveryEvent::Found(peer)) if peer.addr.port() == PORT => break peer,
                Some(_) => {}
                None => panic!("mDNS browse closed before finding local advertisement"),
            }
        }
    })
    .await
    .expect("local advertisement was not discovered");

    assert_eq!(peer.addr.port(), PORT);
}
