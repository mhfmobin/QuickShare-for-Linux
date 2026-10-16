//! Bluetooth LE "wake-up" advert. Android only announces itself over mDNS after hearing this,
//! so it's what makes phones (set to Everyone) appear when we want to send.
//! We never power the adapter on ourselves; when Bluetooth is off we just report it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::{Event, Inner};

const SERVICE: bluer::Uuid = bluer::Uuid::from_u128(0x0000fe2c_0000_1000_8000_00805f9b34fb);

fn advert() -> bluer::adv::Advertisement {
    // fc 12 8e 01 42 + 9 zero bytes + 10 random bytes (exactly fills a legacy 31-byte advert)
    let mut data = vec![0xfc, 0x12, 0x8e, 0x01, 0x42];
    data.extend([0; 9]);
    data.extend(rand::random::<[u8; 10]>());
    bluer::adv::Advertisement {
        advertisement_type: bluer::adv::Type::Broadcast,
        service_data: BTreeMap::from([(SERVICE, data)]),
        min_interval: Some(Duration::from_millis(100)),
        max_interval: Some(Duration::from_millis(200)),
        ..Default::default()
    }
}

pub async fn run(inner: Arc<Inner>, mut want: watch::Receiver<bool>) {
    let Ok(session) = bluer::Session::new().await else {
        inner.emit(Event::Bluetooth(false));
        return;
    };
    let (mut handle, mut last) = (None, None);
    let mut busy = inner.busy.subscribe();
    loop {
        let adapter = session.default_adapter().await.ok();
        let powered = match &adapter {
            Some(a) => a.is_powered().await.unwrap_or(false),
            None => false,
        };
        if last != Some(powered) {
            last = Some(powered);
            inner.emit(Event::Bluetooth(powered));
        }
        let on = *want.borrow() && !*busy.borrow();
        if !on || !powered {
            handle = None; // dropping the handle stops advertising
        } else if handle.is_none() {
            handle = match adapter.unwrap().advertise(advert()).await {
                Ok(h) => {
                    eprintln!("qs: bluetooth wake-up advert on");
                    Some(h)
                }
                Err(e) => {
                    eprintln!("qs: bluetooth advert failed: {e}");
                    None
                }
            };
        }
        // ponytail: polls adapter power every 5 s; switch to adapter.events() if that lag matters
        tokio::select! {
            r = want.changed() => if r.is_err() { return },
            r = busy.changed() => if r.is_err() { return },
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
}
