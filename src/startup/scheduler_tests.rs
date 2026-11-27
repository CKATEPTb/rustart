use super::*;
use crate::install::{consent, files};

struct TemporaryTask<'a>(Option<&'a Registration>);

impl Drop for TemporaryTask<'_> {
    fn drop(&mut self) {
        if let Some(record) = self.0
            && let Err(error) = unsafe { record.scheduler.root.DeleteTask(&record.name, 0) }
        {
            eprintln!(
                "Could not delete disabled fixture {:?}: {error}",
                record.name
            );
        }
    }
}

#[test]
#[ignore = "requires opt-in and an already elevated console"]
fn registers_and_removes_disabled_fixture() {
    if std::env::var("RUSTART_TEST_REGISTER_DISABLED_TASK").as_deref() != Ok("1") {
        return;
    }
    assert!(
        consent::elevated().unwrap(),
        "run from an already elevated console"
    );
    assert!(
        files::installed_image_is_protected().unwrap(),
        "a protected Rustart installation is required"
    );
    let sid = crate::startup::consent::sid().unwrap();
    let scheduler = Scheduler::connect().unwrap();
    let guid = unsafe { CoCreateGuid() }.unwrap().to_u128();
    let name = BSTR::from(format!("Rustart.Test.DisabledAutostart.{guid:032x}"));
    assert!(scheduler.lookup(&name).unwrap().is_none());
    let record = Registration {
        task: None,
        name,
        exe: installed_exe().unwrap(),
        scheduler,
    };
    assert!(
        !record
            .exe
            .as_os_str()
            .encode_wide()
            .any(|c| c == b'%' as u16)
    );
    let task = definition(&record.scheduler.service, &sid, &record.exe).unwrap();
    unsafe {
        // Both the task and its only trigger are disabled before registration.
        let settings = task.Settings().unwrap();
        settings.SetEnabled(false.into()).unwrap();
        assert!(
            !property(|out| settings.AllowDemandStart(out))
                .unwrap()
                .as_bool()
        );
        task.Triggers()
            .unwrap()
            .get_Item(1)
            .unwrap()
            .SetEnabled(false.into())
            .unwrap();
    }
    register(&record, &sid, &task, TASK_CREATE).unwrap();
    let mut cleanup = TemporaryTask(Some(&record));
    let registered = record.scheduler.lookup(&record.name).unwrap().unwrap();
    unsafe {
        assert!(!registered.Enabled().unwrap().as_bool());
        let copy = record.scheduler.service.NewTask(0).unwrap();
        copy.SetXmlText(&registered.Xml().unwrap()).unwrap();
        assert!(matches_definition(&copy, &sid, &record.exe).unwrap());
        let trigger = copy.Triggers().unwrap().get_Item(1).unwrap();
        let settings = copy.Settings().unwrap();
        assert!(!property(|out| trigger.Enabled(out)).unwrap().as_bool());
        assert!(!property(|out| settings.Enabled(out)).unwrap().as_bool());
        assert!(
            !property(|out| settings.AllowDemandStart(out))
                .unwrap()
                .as_bool()
        );
        let flags = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let descriptor = registered.GetSecurityDescriptor(flags as i32).unwrap();
        assert!(
            protected_acl(&descriptor),
            "unexpected registered task ACL {descriptor:?}"
        );
        record.scheduler.root.DeleteTask(&record.name, 0).unwrap();
    }
    cleanup.0 = None;
    assert!(record.scheduler.lookup(&record.name).unwrap().is_none());
}
