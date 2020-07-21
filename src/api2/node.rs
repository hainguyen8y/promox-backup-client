use std::net::TcpListener;
use std::os::unix::io::AsRawFd;

use anyhow::{bail, format_err, Error};
use futures::{
    future::{FutureExt, TryFutureExt},
    try_join,
};
use hyper::body::Body;
use hyper::http::request::Parts;
use hyper::upgrade::Upgraded;
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

use proxmox::api::router::{Router, SubdirMap};
use proxmox::api::{
    api, schema::*, ApiHandler, ApiMethod, ApiResponseFuture, Permission, RpcEnvironment,
};
use proxmox::list_subdirs_api_method;
use proxmox::tools::websocket::WebSocket;
use proxmox::{identity, sortable};

use crate::api2::types::*;
use crate::config::acl::PRIV_SYS_CONSOLE;
use crate::server::WorkerTask;
use crate::tools;

pub mod disks;
pub mod dns;
mod journal;
pub mod network;
pub(crate) mod rrd;
mod services;
mod status;
mod subscription;
mod apt;
mod syslog;
pub mod tasks;
mod time;

pub const SHELL_CMD_SCHEMA: Schema = StringSchema::new("The command to run.")
    .format(&ApiStringFormat::Enum(&[
        EnumEntry::new("login", "Login"),
        EnumEntry::new("upgrade", "Upgrade"),
    ]))
    .schema();

#[api(
    protected: true,
    input: {
        properties: {
            node: {
                schema: NODE_SCHEMA,
            },
            cmd: {
                schema: SHELL_CMD_SCHEMA,
                optional: true,
            },
        },
    },
    returns: {
        type: Object,
        description: "Object with the user, ticket, port and upid",
        properties: {
            user: {
                description: "",
                type: String,
            },
            ticket: {
                description: "",
                type: String,
            },
            port: {
                description: "",
                type: String,
            },
            upid: {
                description: "",
                type: String,
            },
        }
    },
    access: {
        description: "Restricted to users on realm 'pam'",
        permission: &Permission::Privilege(&["nodes","{node}"], PRIV_SYS_CONSOLE, false),
    }
)]
/// Call termproxy and return shell ticket
async fn termproxy(
    node: String,
    cmd: Option<String>,
    _param: Value,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {
    let userid = rpcenv
        .get_user()
        .ok_or_else(|| format_err!("unknown user"))?;
    let (username, realm) = crate::auth::parse_userid(&userid)?;

    if realm != "pam" {
        bail!("only pam users can use the console");
    }

    let path = format!("/nodes/{}", node);

    // use port 0 and let the kernel decide which port is free
    let listener = TcpListener::bind("localhost:0")?;
    let port = listener.local_addr()?.port();

    let ticket = tools::ticket::assemble_term_ticket(
        crate::auth_helpers::private_auth_key(),
        &userid,
        &path,
        port,
    )?;

    let mut command = Vec::new();
    match cmd.as_ref().map(|x| x.as_str()) {
        Some("login") | None => {
            command.push("login");
            if userid == "root@pam" {
                command.push("-f");
                command.push("root");
            }
        }
        Some("upgrade") => {
            bail!("upgrade is not supported yet");
        }
        _ => bail!("invalid command"),
    };

    let upid = WorkerTask::spawn(
        "termproxy",
        None,
        &username,
        false,
        move |worker| async move {
            // move inside the worker so that it survives and does not close the port
            // remove CLOEXEC from listenere so that we can reuse it in termproxy
            let fd = listener.as_raw_fd();
            let mut flags = match fcntl(fd, FcntlArg::F_GETFD) {
                Ok(bits) => FdFlag::from_bits_truncate(bits),
                Err(err) => bail!("could not get fd: {}", err),
            };
            flags.remove(FdFlag::FD_CLOEXEC);
            if let Err(err) = fcntl(fd, FcntlArg::F_SETFD(flags)) {
                bail!("could not set fd: {}", err);
            }

            let mut arguments: Vec<&str> = Vec::new();
            let fd_string = fd.to_string();
            arguments.push(&fd_string);
            arguments.extend_from_slice(&[
                "--path",
                &path,
                "--perm",
                "Sys.Console",
                "--authport",
                "82",
                "--port-as-fd",
                "--",
            ]);
            arguments.extend_from_slice(&command);

            let mut cmd = tokio::process::Command::new("/usr/bin/termproxy");

            cmd.args(&arguments);
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());

            let mut child = cmd.spawn().expect("error executing termproxy");

            let stdout = child.stdout.take().expect("no child stdout handle");
            let stderr = child.stderr.take().expect("no child stderr handle");

            let worker_stdout = worker.clone();
            let stdout_fut = async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Some(line) = reader.next_line().await? {
                    worker_stdout.log(line);
                }
                Ok(())
            };

            let worker_stderr = worker.clone();
            let stderr_fut = async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Some(line) = reader.next_line().await? {
                    worker_stderr.warn(line);
                }
                Ok(())
            };

            let (exit_code, _, _) = try_join!(child, stdout_fut, stderr_fut)?;
            if !exit_code.success() {
                match exit_code.code() {
                    Some(code) => bail!("termproxy exited with {}", code),
                    None => bail!("termproxy exited by signal"),
                }
            }

            Ok(())
        },
    )?;

    Ok(json!({
        "user": username,
        "ticket": ticket,
        "port": port,
        "upid": upid,
    }))
}

#[sortable]
pub const API_METHOD_WEBSOCKET: ApiMethod = ApiMethod::new(
    &ApiHandler::AsyncHttp(&upgrade_to_websocket),
    &ObjectSchema::new(
        "Upgraded to websocket",
        &sorted!([
            ("node", false, &NODE_SCHEMA),
            (
                "vncticket",
                false,
                &StringSchema::new("Terminal ticket").schema()
            ),
            ("port", false, &IntegerSchema::new("Terminal port").schema()),
        ]),
    ),
)
.access(
    Some("The user needs Sys.Console on /nodes/{node}."),
    &Permission::Privilege(&["nodes", "{node}"], PRIV_SYS_CONSOLE, false),
);

fn upgrade_to_websocket(
    parts: Parts,
    req_body: Body,
    param: Value,
    _info: &ApiMethod,
    rpcenv: Box<dyn RpcEnvironment>,
) -> ApiResponseFuture {
    async move {
        let username = rpcenv.get_user().unwrap();
        let node = tools::required_string_param(&param, "node")?.to_owned();
        let path = format!("/nodes/{}", node);
        let ticket = tools::required_string_param(&param, "vncticket")?.to_owned();
        let port: u16 = tools::required_integer_param(&param, "port")? as u16;

        // will be checked again by termproxy
        tools::ticket::verify_term_ticket(
            crate::auth_helpers::public_auth_key(),
            &username,
            &path,
            port,
            &ticket,
        )?;

        let (ws, response) = WebSocket::new(parts.headers)?;

        tokio::spawn(async move {
            let conn: Upgraded = match req_body.on_upgrade().map_err(Error::from).await {
                Ok(upgraded) => upgraded,
                _ => bail!("error"),
            };

            let local = tokio::net::TcpStream::connect(format!("localhost:{}", port)).await?;
            ws.serve_connection(conn, local).await
        });

        Ok(response)
    }
    .boxed()
}

pub const SUBDIRS: SubdirMap = &[
    ("apt", &apt::ROUTER),
    ("disks", &disks::ROUTER),
    ("dns", &dns::ROUTER),
    ("journal", &journal::ROUTER),
    ("network", &network::ROUTER),
    ("rrd", &rrd::ROUTER),
    ("services", &services::ROUTER),
    ("status", &status::ROUTER),
    ("subscription", &subscription::ROUTER),
    ("syslog", &syslog::ROUTER),
    ("tasks", &tasks::ROUTER),
    ("termproxy", &Router::new().post(&API_METHOD_TERMPROXY)),
    ("time", &time::ROUTER),
    (
        "vncwebsocket",
        &Router::new().upgrade(&API_METHOD_WEBSOCKET),
    ),
];

pub const ROUTER: Router = Router::new()
    .get(&list_subdirs_api_method!(SUBDIRS))
    .subdirs(SUBDIRS);
