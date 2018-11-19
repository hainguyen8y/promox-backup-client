#[macro_use]
extern crate apitest;

use std::collections::HashMap;

use apitest::api::schema::*;
use apitest::api::router::*;
use apitest::api::config::*;
use apitest::api::server::*;
use apitest::getopts;

//use failure::*;
use lazy_static::lazy_static;

use futures::future::Future;

use hyper;

fn main() {
    println!("Proxmox REST Server example.");

    let schema = parameter!{
        name => ApiString!{ optional => true }
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    match getopts::parse_arguments(&args, &vec![], &schema) {
        Ok((options, rest)) => {
            println!("Got Options: {}", options);
            println!("Remaining Arguments: {:?}", rest);
        }
        Err(err) => {
            eprintln!("Unable to parse arguments:\n{}", err);
            std::process::exit(-1);
        }
    }

    let addr = ([127, 0, 0, 1], 8007).into();

    lazy_static!{
       static ref ROUTER: Router = apitest::api3::router();
    }

    let mut config = ApiConfig::new("/var/www", &ROUTER);

    // add default dirs which includes jquery and bootstrap
    // my $base = '/usr/share/libpve-http-server-perl';
    // add_dirs($self->{dirs}, '/css/' => "$base/css/");
    // add_dirs($self->{dirs}, '/js/' => "$base/js/");
    // add_dirs($self->{dirs}, '/fonts/' => "$base/fonts/");
    config.add_alias("novnc", "/usr/share/novnc-pve");
    config.add_alias("extjs", "/usr/share/javascript/extjs");
    config.add_alias("fontawesome", "/usr/share/fonts-font-awesome");
    config.add_alias("xtermjs", "/usr/share/pve-xtermjs");
    config.add_alias("widgettoolkit", "/usr/share/javascript/proxmox-widget-toolkit");

    let rest_server = RestServer::new(config);

    let server = hyper::Server::bind(&addr)
        .serve(rest_server)
        .map_err(|e| eprintln!("server error: {}", e));


    // Run this server for... forever!
    hyper::rt::run(server);
}
