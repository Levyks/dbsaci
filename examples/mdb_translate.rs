//! Ad-hoc: print `oracle_to_mariadb` output for statements passed as args.
//! `cargo run --example mdb_translate -- "SELECT LISTAGG(name, ', ') WITHIN GROUP (ORDER BY id) FROM people"`
fn main() {
    for sql in std::env::args().skip(1) {
        match dbsaci::translate::oracle_to_mariadb(&sql) {
            Ok(out) => println!("IN : {sql}\nOUT: {out}\n"),
            Err(e) => println!("IN : {sql}\nERR: {e}\n"),
        }
    }
}
