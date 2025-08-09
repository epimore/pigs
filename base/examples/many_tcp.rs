// use base::net;
// use std::net::SocketAddr;
// use std::str::FromStr;
// use base::net::state::Zip;
//
// //cmd: cargo run --example many_tcp --features net
// #[tokio::main]
// async fn main() {
//     let (tx1, mut rx1) = net::init_net(net::state::Protocol::TCP, SocketAddr::from_str("0.0.0.0:18887").unwrap()).await.unwrap();
//     let (tx2, mut rx2) = net::init_net(net::state::Protocol::TCP, SocketAddr::from_str("0.0.0.0:18888").unwrap()).await.unwrap();
//     let (tx3, mut rx3) = net::init_net(net::state::Protocol::TCP, SocketAddr::from_str("0.0.0.0:18889").unwrap()).await.unwrap();
//     tokio::spawn(
//         async move {
//             while let Some(zip) = rx1.recv().await {
//                 match zip {
//                     Zip::Data(ref package) => {
//                         println!("association = {:?} - data_size: {}", package.get_association(), package.get_data().len());
//                         let _ = tx1.clone().send(zip).await;
//                     }
//                     Zip::Event(_) => {}
//                 }
//             }
//         }
//     );
//     tokio::spawn(
//         async move {
//             while let Some(zip) = rx2.recv().await {
//                 match zip {
//                     Zip::Data(ref package) => {
//                         println!("association = {:?} - data_size: {}", package.get_association(), package.get_data().len());
//                         let _ = tx2.clone().send(zip).await;
//                     }
//                     Zip::Event(_) => {}
//                 }
//             }
//         }
//     );
//     while let Some(zip) = rx3.recv().await {
//         match zip {
//             Zip::Data(ref package) => {
//                 println!("association = {:?} - data_size: {}", package.get_association(), package.get_data().len());
//                 let _ = tx3.clone().send(zip).await;
//             }
//             Zip::Event(_) => {}
//         }
//     }
// }
fn main() {}