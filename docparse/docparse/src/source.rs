use crate::error::Result;
use crate::model::Page;

/// One page at a time, pulled on demand.
///
/// Returning a whole document would make peak memory scale with input
/// size, which is the structural reason the Python pipeline cannot
/// process a large PDF on a small machine. Pulling makes the memory
/// ceiling a property of the architecture rather than a tuning knob.
///
/// `None` means the document is finished. `Some(Err(..))` means this
/// page failed; the caller decides whether to stop.
pub trait Source {
    fn next_page(&mut self) -> Option<Result<Page>>;
}

/// A boxed source is itself a source.
///
/// Rust does not give you this for free: `Box<dyn Source>` is a distinct
/// type from `dyn Source`, and the CLI needs to pass a boxed, runtime-chosen
/// backend to a function that is generic over `S: Source`. This impl is what
/// makes that work. `?Sized` is what lets it cover `Box<dyn Source>` and not
/// just `Box<ConcreteSource>`.
impl<S: Source + ?Sized> Source for Box<S> {
    fn next_page(&mut self) -> Option<Result<Page>> {
        (**self).next_page()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PageOrigin;

    struct OnePage(Option<Page>);

    impl Source for OnePage {
        fn next_page(&mut self) -> Option<Result<Page>> {
            self.0.take().map(Ok)
        }
    }

    fn pull_one<S: Source>(mut s: S) -> Option<Page> {
        s.next_page().map(|r| r.unwrap())
    }

    #[test]
    fn boxed_dyn_source_is_itself_a_source() {
        let page = Page {
            number: 1,
            width: 0.0,
            height: 0.0,
            origin: PageOrigin::Synthesized,
            blocks: vec![],
        };
        let mut boxed: Box<dyn Source> = Box::new(OnePage(Some(page.clone())));

        assert_eq!(boxed.next_page().unwrap().unwrap(), page);
        assert!(boxed.next_page().is_none());

        // Passing the box to a generic `S: Source` function is the real use.
        let boxed: Box<dyn Source> = Box::new(OnePage(Some(page.clone())));
        assert_eq!(pull_one(boxed), Some(page));
    }
}
