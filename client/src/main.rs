#![deny(warnings)]
#[macro_use]
extern crate log;
extern crate pretty_env_logger;
use clap::Parser;
use hyper::header;
use once_cell::sync::Lazy;
use prost::Message;
use std::net::ToSocketAddrs;
use std::process;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use sysinfo::{System, SystemExt};
use tokio::time;

use stat_common::server_status::{IpInfo, StatRequest, SysInfo};
type GenericError = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, GenericError>;
mod grpc;
mod ip_api;
mod status;
mod sys_info;

const INTERVAL_MS: u64 = 1000;
static CU: &str = "cu.tz.cloudcpp.com:80";
static CT: &str = "ct.tz.cloudcpp.com:80";
static CM: &str = "cm.tz.cloudcpp.com:80";

#[derive(Default)]
pub struct ClientConfig {
    ip_info: Option<IpInfo>,
    sys_info: Option<SysInfo>,
}

pub static G_CONFIG: Lazy<Mutex<ClientConfig>> = Lazy::new(|| Mutex::new(ClientConfig::default()));

// https://docs.rs/clap/latest/clap/_derive/index.html#command-attributes
#[derive(Parser, Debug, Clone)]
#[clap(author, version = env!("APP_VERSION"), about, long_about = None)]
pub struct Args {
    #[clap(
        short,
        long,
        value_parser,
        env = "SSR_ADDR",
        default_value = "http://127.0.0.1:8080/report"
    )]
    addr: String,
    #[clap(short, long, value_parser, env = "SSR_USER", default_value = "h1", help = "username")]
    user: String,
    #[clap(short, long, value_parser, env = "SSR_PASS", default_value = "p1", help = "password")]
    pass: String,
    #[clap(
        short = 'n',
        long,
        value_parser,
        env = "SSR_VNSTAT",
        help = "enable vnstat, default:false"
    )]
    vnstat: bool,
    #[clap(
        long = "disable-tupd",
        value_parser,
        env = "SSR_DISABLE_TUPD",
        help = "disable t/u/p/d, default:false"
    )]
    disable_tupd: bool,
    #[clap(
        long = "disable-ping",
        value_parser,
        env = "SSR_DISABLE_PING",
        help = "disable ping, default:false"
    )]
    disable_ping: bool,
    #[clap(
        long = "disable-extra",
        value_parser,
        env = "SSR_DISABLE_EXTRA",
        help = "disable extra info report, default:false"
    )]
    disable_extra: bool,
    #[clap(long = "ct", value_parser, env = "SSR_CT_ADDR", default_value = CT, help = "China Telecom probe addr")]
    ct_addr: String,
    #[clap(long = "cm", value_parser, env = "SSR_CM_ADDR", default_value = CM, help = "China Mobile probe addr")]
    cm_addr: String,
    #[clap(long = "cu", value_parser, env = "SSR_CU_ADDR", default_value = CU, help = "China Unicom probe addr")]
    cu_addr: String,
    #[clap(long = "ip-info", value_parser, help = "show ip info, default:false")]
    ip_info: bool,
    #[clap(long = "json", value_parser, help = "use json protocol, default:false")]
    json: bool,
    #[clap(short = '6', value_parser, long = "ipv6", help = "ipv6 only, default:false")]
    ipv6: bool,
    // for group
    #[clap(short, long, value_parser, env = "SSR_GID", default_value = "", help = "group id")]
    gid: String,
    #[clap(
        long = "alias",
        value_parser,
        env = "SSR_ALIAS",
        default_value = "unknown",
        help = "alias for host"
    )]
    alias: String,
    #[clap(
        short,
        long,
        value_parser,
        env = "SSR_WEIGHT",
        default_value = "0",
        help = "weight for rank"
    )]
    weight: u64,
    #[clap(
        long = "disable-notify",
        env = "SSR_DISABLE_NOTIFY",
        value_parser,
        help = "disable notify, default:false"
    )]
    disable_notify: bool,
    #[clap(
        short = 't',
        long = "type",
        value_parser,
        env = "SSR_TYPE",
        default_value = "",
        help = "host type"
    )]
    host_type: String,
    #[clap(long, value_parser, env = "SSR_LOC", default_value = "", help = "location")]
    location: String,
    #[clap(short = 'd', long = "debug", env = "SSR_DEBUG", help = "debug mode, default:false")]
    debug: bool,
    #[clap(
        short = 'i',
        long = "iface",
        value_parser,
        env = "SSR_IFACE",
        default_values_t = Vec::<String>::new(),
        value_delimiter = ',',
        require_delimiter = true,
        help = "iface list, eg: eth0,eth1"
    )]
    iface: Vec<String>,
    #[clap(
        short = 'e',
        long = "exclude-iface",
        value_parser,
        env = "SSR_EXCLUDE_IFACE",
        default_value = "lo,docker,vnet,veth,vmbr,kube,br-",
        value_delimiter = ',',
        help = "exclude iface"
    )]
    exclude_iface: Vec<String>,
}

pub fn skip_iface(name: &str, args: &Args) -> bool {
    if !args.iface.is_empty() {
        if args.iface.iter().any(|fa| name.eq(fa)) {
            return false;
        }
        return true;
    }
    if args.exclude_iface.iter().any(|sk| name.contains(sk)) {
        return true;
    }
    false
}

fn sample_all(args: &Args, stat_base: &StatRequest) -> StatRequest {
    // dbg!(&stat_base);
    let mut stat_rt = stat_base.clone();

    #[cfg(all(feature = "native", not(feature = "sysinfo")))]
    status::sample(args, &mut stat_rt);
    #[cfg(all(feature = "sysinfo", not(feature = "native")))]
    sys_info::sample(args, &mut stat_rt);

    stat_rt.latest_ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

    if !args.disable_extra {
        if let Ok(o) = G_CONFIG.lock() {
            if let Some(ip_info) = o.ip_info.as_ref() {
                stat_rt.ip_info = Some(ip_info.clone());
            }
            if let Some(sys_info) = o.sys_info.as_ref() {
                stat_rt.sys_info = Some(sys_info.clone());
            }
        }
    }

    stat_rt
}

fn http_report(args: &Args, stat_base: &mut StatRequest) -> Result<()> {
    let mut domain = args.addr.split('/').collect::<Vec<&str>>()[2].to_owned();
    if !domain.contains(':') {
        if args.addr.contains("https") {
            domain = format!("{}:443", domain);
        } else {
            domain = format!("{}:80", domain);
        }
    }
    let tcp_addr = domain.to_socket_addrs()?.next().unwrap();
    let (ipv4, ipv6) = (tcp_addr.is_ipv4(), tcp_addr.is_ipv6());
    if ipv4 {
        stat_base.online4 = ipv4;
    }
    if ipv6 {
        stat_base.online6 = ipv6;
    }

    let http_client = reqwest::Client::builder()
        .pool_max_idle_per_host(1)
        .connect_timeout(Duration::from_secs(5))
        .user_agent(format!("{}/{}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION")))
        .build()?;
    loop {
        let stat_rt = sample_all(args, stat_base);

        let body_data: Option<Vec<u8>>;
        let mut content_type = "application/octet-stream";
        if args.json {
            let data = serde_json::to_string(&stat_rt)?;
            trace!("json_str => {:?}", serde_json::to_string(&data)?);
            body_data = Some(data.into());
            content_type = "application/json";
        } else {
            let buf = stat_rt.encode_to_vec();
            body_data = Some(buf);
            // content_type = "application/octet-stream";
        }
        // byte 581, json str 1281
        // dbg!(&body_data.as_ref().unwrap().len());

        let client = http_client.clone();
        let url = args.addr.to_string();
        let auth_pass = args.pass.to_string();
        let auth_user: String;
        let ssr_auth: &str;
        if args.gid.is_empty() {
            auth_user = args.user.to_string();
            ssr_auth = "single";
        } else {
            auth_user = args.gid.to_string();
            ssr_auth = "group";
        }

        // http
        tokio::spawn(async move {
            match client
                .post(&url)
                .basic_auth(auth_user, Some(auth_pass))
                .timeout(Duration::from_secs(3))
                .header(header::CONTENT_TYPE, content_type)
                .header("ssr-auth", ssr_auth)
                .body(body_data.unwrap())
                .send()
                .await
            {
                Ok(resp) => {
                    info!("report resp => {:?}", resp);
                }
                Err(err) => {
                    error!("report error => {:?}", err);
                }
            }
        });

        thread::sleep(Duration::from_millis(INTERVAL_MS));
    }
}

async fn refresh_ip_info(args: &Args) {
    // refresh/1 hour
    let mut interval = time::interval(time::Duration::from_secs(3600));
    loop {
        info!("get ip info from ip-api.com");
        match ip_api::get_ip_info(args.ipv6).await {
            Ok(ip_info) => {
                info!("refresh_ip_info succ => {:?}", ip_info);
                if let Ok(mut o) = G_CONFIG.lock() {
                    o.ip_info = Some(ip_info);
                }
            }
            Err(err) => {
                error!("refresh_ip_info error => {:?}", err);
            }
        }

        interval.tick().await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();
    let mut args = Args::parse();
    args.iface.retain(|e| !e.trim().is_empty());
    args.exclude_iface.retain(|e| !e.trim().is_empty());
    if args.debug {
        dbg!(&args);
    }

    if args.ip_info {
        let info = ip_api::get_ip_info(args.ipv6).await?;
        dbg!(info);
        process::exit(0);
    }

    // support check
    if !System::IS_SUPPORTED {
        panic!("当前系统不支持，请切换到Python跨平台版本!");
    }

    let sys_info = sys_info::collect_sys_info(&args);
    let sys_info_json = serde_json::to_string(&sys_info)?;
    let sys_id = sys_info::gen_sys_id(&sys_info);
    eprintln!("sys id: {}", sys_id);
    eprintln!("sys info: {}", sys_info_json);

    if let Ok(mut o) = G_CONFIG.lock() {
        o.sys_info = Some(sys_info);
    }

    // use native
    #[cfg(all(feature = "native", not(feature = "sysinfo")))]
    {
        eprintln!("enable feature native");
        status::start_cpu_percent_collect_t();
        status::start_net_speed_collect_t(&args);
    }

    // use sysinfo
    #[cfg(all(feature = "sysinfo", not(feature = "native")))]
    {
        eprintln!("enable feature sysinfo");
        sys_info::start_cpu_percent_collect_t();
        sys_info::start_net_speed_collect_t();
    }

    status::start_all_ping_collect_t(&args);
    let (ipv4, ipv6) = status::get_network();
    eprintln!("get_network (ipv4, ipv6) => ({}, {})", ipv4, ipv6);

    if !args.disable_extra {
        // refresh ip info
        let args_1 = args.clone();
        tokio::spawn(async move { refresh_ip_info(&args_1).await });
    }

    let mut stat_base = StatRequest {
        name: args.user.to_string(),
        frame: "data".to_string(),
        online4: ipv4,
        online6: ipv6,
        vnstat: args.vnstat,
        weight: args.weight,
        notify: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
        ..Default::default()
    };
    if !args.gid.is_empty() {
        stat_base.gid = args.gid.to_owned();
        if stat_base.name.eq("h1") {
            stat_base.name = sys_id;
        }
        if args.alias.eq("unknown") {
            args.alias = stat_base.name.to_owned();
        } else {
            stat_base.alias = args.alias.to_owned();
        }
    }
    if args.disable_notify {
        stat_base.notify = false;
    }
    if !args.host_type.is_empty() {
        stat_base.r#type = args.host_type.to_owned();
    }
    if !args.location.is_empty() {
        stat_base.location = args.location.to_owned();
    }
    // dbg!(&stat_base);

    if args.addr.starts_with("http") {
        let result = http_report(&args, &mut stat_base);
        dbg!(&result);
    } else if args.addr.starts_with("grpc") {
        let result = grpc::report(&args, &mut stat_base).await;
        dbg!(&result);
    } else {
        eprint!("invalid addr scheme!");
    }

    Ok(())
}
