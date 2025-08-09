// use std::net::SocketAddr;
// use std::str::FromStr;
// use bytes::Bytes;
// use log::error;
// use base::net;
// use base::net::state::Zip;
// use exception::TransError;
//
// //cmd: cargo run --example single_std_all --features net
// #[tokio::main]
// async fn main() {
//     let tu = net::sdx::listen(net::state::Protocol::ALL, SocketAddr::from_str("0.0.0.0:18889").unwrap()).unwrap();
//     let (tx, mut rx) = net::sdx::run_by_tokio(tu).await.unwrap();
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