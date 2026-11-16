//! Ad-hoc: print `oracle_to_mariadb` output for statements passed as args.
//!
//! `cargo run --example mdb_translate -- "SELECT LISTAGG(name, ', ') WITHIN GROUP (ORDER BY id) FROM people"`
//!
//! Flags (before the statements):
//! * `--route` — translate with dictionary routing on (`dbsaci_sys`);
//! * `--raw` — print only the translation, batch statements separated by `;\n`
//!   (pipe into the `mariadb` client);
//! * `--helpers` — print the helper routines the output may call, `;;`-separated.
use dbsaci::translate::mariadb::{HELPERS, MariaDbContext, oracle_to_mariadb_ctx};

fn main() {
    let mut route = false;
    let mut raw = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--route" => route = true,
            "--raw" => raw = true,
            "--helpers" => {
                for (_, ddl) in HELPERS {
                    println!("{ddl}\n;;");
                }
            }
            sql => {
                let cx = MariaDbContext {
                    route_dictionary: route,
                    ..MariaDbContext::default()
                };
                match oracle_to_mariadb_ctx(sql, &cx) {
                    Ok(out) if raw => println!(
                        "{};",
                        out.replace(dbsaci::translate::MARIADB_BATCH_SEP, ";\n")
                    ),
                    Ok(out) => println!("IN : {sql}\nOUT: {out}\n"),
                    Err(e) => println!("IN : {sql}\nERR: {e}\n"),
                }
            }
        }
    }
}
