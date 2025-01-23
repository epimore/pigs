// use common::net;
// use std::net::SocketAddr;
// use std::str::FromStr;
// use bytes::Bytes;
// use log::error;
// use exception::TransError;
// use common::net::state::{Zip};
//
// //cmd: cargo run --example single_all --features net
// #[tokio::main]
// async fn main() {
//     let (tx, mut rx) = net::init_net(net::state::Protocol::ALL, SocketAddr::from_str("0.0.0.0:18889").unwrap()).await.unwrap();
//     let mut i = 0;
//     while let Some(zip) = rx.recv().await {
//         match zip {
//             Zip::Data(mut package) => {
//                 println!("association = {:?} - data_size: {}", package.get_association(), package.get_data().len());
//                 i += 1;
//                 package.set_data(Bytes::from(format!("abc - {}", i)));
//                 println!("{:?}",&package);
//                 let _ = tx.clone().send(Zip::build_data(package)).await.hand_log(|msg|error!("{msg}"));
//             }
//             Zip::Event(_) => {}
//         }
//     }
// }
fn main() {}