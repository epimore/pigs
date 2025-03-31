use std::collections::HashMap;
use log::{debug, error};
use exception::{GlobalError, GlobalResult};

const D_SIZE: usize = 10;
const A_SIZE: usize = 52;
const D_DIC: [char; D_SIZE] = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9'];
//按键盘从上至下，从左到右形成小写、大写字母字典表
const A_DIC: [char; A_SIZE] = ['q', 'a', 'z', 'w', 's', 'x', 'e', 'd', 'c', 'r', 'f', 'v', 't', 'g', 'b', 'y', 'h', 'n', 'u', 'j', 'm', 'i', 'k', 'o', 'l', 'p', 'Q', 'A', 'Z', 'W', 'S', 'X', 'E', 'D', 'C', 'R', 'F', 'V', 'T', 'G', 'B', 'Y', 'H', 'N', 'U', 'J', 'M', 'I', 'K', 'O', 'L', 'P'];

/// 将10进制纯数字字符串，按照字典表生成新的字符串；长度压缩到58%；对称压缩
/// en 编码
/// de 解码
pub fn en(digit_str: &str) -> GlobalResult<String> {
    let mut tmp_key0 = String::new();
    for ch in digit_str.chars() {
        let digit = ch.to_digit(10).ok_or_else(|| GlobalError::new_sys_error("Invalid digit_str", |msg| error!("{msg}")))?;
        tmp_key0.push_str(&format!("{:04b}", digit));
    }
    let remainder = tmp_key0.len() % 9;
    let pad_len = 9 - remainder;
    if remainder != 0 {
        tmp_key0 = format!("{}{}", "0".repeat(pad_len), tmp_key0);
    }
    let header = A_DIC[pad_len];
    let mut dst_key = String::new();
    dst_key.push(header);
    for chunk in tmp_key0.chars().collect::<Vec<_>>().chunks(9) {
        let bin_str: String = chunk.iter().collect();
        //此处不会报错
        let val = usize::from_str_radix(&bin_str, 2).expect("Invalid binary group");
        let circle = val / 52;
        let index = val % 52;
        if circle > 0 {
            dst_key.push(D_DIC[circle]);
        }
        dst_key.push(A_DIC[index]);
    }
    debug!("Binary: {}", tmp_key0);
    Ok(dst_key)
}

pub fn de(num62: &str) -> GlobalResult<String> {
    let mut ori_chars = num62.chars();
    let header = ori_chars.next().ok_or_else(|| GlobalError::new_sys_error("Too short to miss header", |msg| error!("{msg}")))?;
    let mut a_dic_map: HashMap<char, usize> = HashMap::new();
    for (i, ch) in A_DIC.iter().enumerate() {
        a_dic_map.insert(*ch, i);
    }
    let pad_len = *a_dic_map.get(&header).ok_or_else(|| GlobalError::new_sys_error("Illegal alphabet character by header", |msg| error!("{msg}")))?;

    let mut d_dic_map: HashMap<char, usize> = HashMap::new();
    for (i, ch) in D_DIC.iter().enumerate() {
        d_dic_map.insert(*ch, i);
    }
    let mut binary_str = String::new();
    let chars: Vec<char> = num62.chars().collect();
    let mut i = 1; //跳过header
    while i < chars.len() {
        let mut circle = 0;
        let a_ch;

        if chars[i].is_digit(10) {
            circle = *d_dic_map.get(&chars[i]).ok_or_else(|| GlobalError::new_sys_error("Illegal digit character", |msg| error!("{msg}")))?;
            i += 1;
            if i >= chars.len() {
                return Err(GlobalError::new_sys_error("Digit num cannot be at the end", |msg| error!("{msg}")));
            }
            a_ch = chars[i];
        } else {
            a_ch = chars[i];
        }
        let index = *a_dic_map.get(&a_ch).ok_or_else(|| GlobalError::new_sys_error("Illegal alphabet character", |msg| error!("{msg}")))?;
        let val = circle * 52 + index;
        binary_str.push_str(&format!("{:09b}", val)); // 保证9位
        i += 1;
    }
    debug!("Recovered binary: {}", binary_str);
    binary_str = binary_str.chars().skip(pad_len).collect();

    // 按4位切回十进制字符串
    let mut result = String::new();
    for chunk in binary_str.chars().collect::<Vec<_>>().chunks(4) {
        let bin = chunk.iter().collect::<String>();
        let num = u32::from_str_radix(&bin, 2).expect("Invalid binary group");
        result.push(std::char::from_digit(num, 10).expect("Invalid number"));
    }
    Ok(result)
}

#[cfg(test)]
mod test {
    use crate::utils::dig62::{de, en};

    #[test]
    fn test_en() {
        let device_id = "34020000001110000001";
        let channel_id = "34020000001320000101";
        let ssrc = "1100000001";
        let dig_str = format!("{}{}{}", device_id, channel_id, ssrc);
        let result = en(&dig_str);
        println!("{:?}", result);

        let result1 = de(&result.unwrap());
        println!("{:?}", result1);
        assert_eq!(dig_str, result1.unwrap());
    }
}