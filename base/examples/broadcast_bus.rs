
use std::{time::Duration};
use tokio::time::sleep;
use base::bus::broadcast::TypedMessageBus;

#[tokio::main]
async fn main() {
    let bus = TypedMessageBus::new();

    // 订阅者1：监听字符串
    let mut sub_str = bus.sub_type_channel::<String>();
    tokio::spawn(async move {
        while let Ok(msg) = sub_str.recv().await {
            println!("[str] received: {}", msg);
        }
    });

    // 订阅者2：监听 u32
    let mut sub_u32 = bus.sub_type_channel::<u32>();
    tokio::spawn(async move {
        while let Ok(msg) = sub_u32.recv().await {
            println!("[u32] received: {}", msg);
        }
    });

    // 订阅者3：监听 User
    let mut sub_user = bus.sub_type_channel::<User>();
    let mut sub_user1 = bus.sub_type_channel::<User>();
    tokio::spawn(async move {
        while let Ok(user) = sub_user.recv().await {
            println!("[User] received: {:?}", user);
        }
    });

    println!("Publishing messages...");
    bus.publish("hello world".to_string()).expect("TODO: panic message");
    bus.publish(42_u32).expect("TODO: panic message");
    bus.publish(User { name: "Alice".into(), age: 30, sex: false }).expect("TODO: panic message");
    if let Ok(user) = sub_user1.try_recv() {
        println!("[User1] received: {:?}", user);
    }
    sleep(Duration::from_secs(2)).await;
    println!("Done.");
}


#[derive(Debug, Default, Clone)]
struct User {
    name: String,
    age: u8,
    sex: bool,
}
