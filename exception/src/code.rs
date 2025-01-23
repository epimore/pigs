/// A保留：0..999;B对外暴露:1000..9999；C系统自用:10000..65535；
/// 1000..1099网络异常
/// 1100..1199数据异常
/// ...
#[allow(unused)]
pub mod err {
    //系统异常退出
    pub const SYS_SUPPER_ERROR_CODE: u16 = 999;
    //网络通信错误父编码
    pub const NET_SUPPER_ERROR_CODE: u16 = 1000;
    //数据错误父编码
    pub const DATA_SUPPER_ERROR_CODE: u16 = 1100;
    //系统自定义的数据错误父编码
    pub const SYSTEM_DATA_ERROR_CODE: u16 = 10000;
}

//super -> NET_SUPPER_ERROR_CODE = 1000
#[allow(unused)]
pub mod net_err {
    //网络通信错误：TCP连接错误
    pub const NET_UNINITIALIZED_ERROR_CODE: u16 = 1001;
    pub const TCP_CONNECT_ERROR_CODE: u16 = 1010;
}

//super -> DATA_SUPPER_ERROR_CODE = 1100
#[allow(unused)]
pub mod conf_err {
    //数据错误：参数配置错误
    pub const CONFIG_ERROR_CODE: u16 = 1101;
}