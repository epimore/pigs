use std::{time::Duration};
use tokio::time::sleep;
use base::bus::mpsc::TypedMessageBus;
use exception::typed::common::MessageBusError;

#[tokio::main]
async fn main() {
    let bus = TypedMessageBus::new();
    let mut str_receiver = bus.sub_type_channel::<String>().unwrap();
    let mut user_receiver = bus.sub_type_channel::<User>().unwrap();
    let mut u8_receiver = bus.sub_type_channel::<u8>().unwrap();
    tokio::spawn(async move {
        if let Ok(msg) = str_receiver.recv().await {
            println!("[str] received: {}", msg);
        }
    });
    tokio::spawn(async move {
        if let Ok(msg) = user_receiver.recv().await {
            println!("[user] received: {:?}", msg);
        }
    });
    tokio::spawn(async move {
        match u8_receiver.recv_with_timeout(Duration::from_secs(1)).await  {
            Ok(msg) => {println!("[u8] received: {:?}", msg);}
            Err(err) => { println!("[u8] receive error: {:?}", err);}
        }
    });
    assert_eq!(bus.publish(42_u32).await.is_err(), true);
    bus.publish("aaaa".to_string()).await.expect("TODO: panic message");
    bus.publish(User { name: "Alice".into(), age: 30, sex: false }).await.expect("TODO: panic message");

    sleep(Duration::from_secs(2)).await;
    match bus.publish(42_u8).await {
        Ok(_) => {}
        Err(err) => { println!("[u8] receive error: {:?}", err);}
    }
    
    assert_eq!(bus.publish(12i8).await.is_err(), true);
    sleep(Duration::from_secs(2)).await;
    let mut i8_receiver = bus.sub_type_channel::<i8>().unwrap();
    if let Ok(msg) = i8_receiver.recv_with_timeout(Duration::from_secs(1)).await {
        println!("[i8] received: {}", msg);
    }
    println!("Done.");
}


#[derive(Debug, Default, Clone)]
struct User {
    name: String,
    age: u8,
    sex: bool,
}
