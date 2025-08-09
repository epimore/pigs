// use base::net;
// use std::net::SocketAddr;
// use std::str::FromStr;
// use base::net::state::Zip;
//
// //cmd: cargo run --example single_tcp --features net
// #[tokio::main]
// async fn main() {
//     let (tx, mut rx) = net::init_net(net::state::Protocol::TCP, SocketAddr::from_str("0.0.0.0:18888").unwrap()).await.unwrap();
//     while let Some(zip) = rx.recv().await {
//         match zip {
//             Zip::Data(ref package) => {
//                 println!("association = {:?} - data_size: {}", package.get_association(), package.get_data().len());
//                 let _ = tx.clone().send(zip).await;
//             }
//             Zip::Event(ref event) => {
//                 println!("event type code = {}",event.get_type_code());
//             }
//         }
//     }
// }
fn main() {}