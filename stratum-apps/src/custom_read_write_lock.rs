use parking_lot::RwLock as InnerRwLock;

#[derive(Debug)]
pub struct RwLock<T: ?Sized>(InnerRwLock<T>);

impl<T> RwLock<T> {
    pub fn read<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        f(&*self.0.read())
    }

    pub fn write<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        f(&mut *self.0.write())
    }
}
