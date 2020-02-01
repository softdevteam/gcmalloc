#[derive(Debug, PartialEq)]
pub enum TraceType {
    Conservative,
    Custom,
}

/// The trait used by the collector to determine how an object's fields should
/// be traced.
pub trait Trace {
    fn trace(&self, _: &mut Vec<usize>) -> TraceType {
        TraceType::Conservative
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trace_derive::Trace;

    #[test]
    fn custom_trace() {
        fn unmask_fn(x: usize) -> *const u8 {
            (x + 1) as *const u8
        }

        #[derive(Trace)]
        struct S {
            #[trace = "unmask_fn"]
            x: usize,
        }

        let s = S { x: 0 };
        let mut trace_stack = Vec::<usize>::new();
        let tt = s.trace(&mut trace_stack);
        assert_eq!(tt, TraceType::Custom);
        assert_eq!(trace_stack[0], 1);
    }

    #[test]
    fn custom_trace_records_all_fields() {
        fn unmask_fn(x: usize) -> *const u8 {
            (x + 1) as *const u8
        }

        struct T;
        #[derive(Trace)]
        struct S<'a> {
            #[trace = "unmask_fn"]
            x: usize,
            _y: *mut u8,
            #[trace]
            z: &'a T,
        }

        let t = T {};
        let s = S {
            x: 0,
            _y: 123 as *mut u8,
            z: &t,
        };
        let mut trace_stack = Vec::<usize>::new();
        let tt = s.trace(&mut trace_stack);

        assert_eq!(tt, TraceType::Custom);
        assert_eq!(trace_stack, &[s.z as *const _ as usize, 1]);
    }
}
