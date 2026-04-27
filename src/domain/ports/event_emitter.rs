use crate::domain::events::AppEvent;

pub trait EventEmitter: Send + Sync {
    fn emit(&self, event: AppEvent);
}

impl<T: EventEmitter + ?Sized> EventEmitter for std::sync::Arc<T> {
    fn emit(&self, event: AppEvent) {
        (**self).emit(event);
    }
}
