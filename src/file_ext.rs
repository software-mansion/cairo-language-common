use cairo_lang_filesystem::{
    db::ext_as_virtual,
    ids::{FileId, FileLongId, VirtualFile},
};
use salsa::Database;

pub trait FileIdExt<'db> {
    fn maybe_as_virtual(&self, db: &'db dyn Database) -> Option<&'db VirtualFile<'db>>;
    fn as_virtual(&self, db: &'db dyn Database) -> &'db VirtualFile<'db>;
}

impl<'db> FileIdExt<'db> for FileId<'db> {
    fn maybe_as_virtual(&self, db: &'db dyn Database) -> Option<&'db VirtualFile<'db>> {
        match self.long(db) {
            FileLongId::OnDisk(_) => None,
            FileLongId::External(ext) => Some(ext_as_virtual(db, *ext)),
            FileLongId::Virtual(vfs) => Some(vfs),
        }
    }

    fn as_virtual(&self, db: &'db dyn Database) -> &'db VirtualFile<'db> {
        self.maybe_as_virtual(db).expect("file can't be OnDisk")
    }
}
