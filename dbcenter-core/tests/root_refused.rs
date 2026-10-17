//! Started as root, the engine says what to do instead of passing on initdb's
//! "cannot be run as root". Only proves anything when the tests run as root,
//! which they do not normally; it is run as root on purpose to check this.

use dbcenter_core::DBCenter;

#[test]
fn running_as_root_explains_what_to_do() {
    if !dbcenter_core::utils::running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    let err = match DBCenter::builder().persist(false).build() {
        Ok(_) => panic!("started as root"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("refuses to run as root"), "{err}");
    assert!(err.contains("sudo -u dbuser"), "{err}");
}
