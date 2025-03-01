use smallvec::SmallVec;
use rle::{HasLength, SplitableSpan, SplitableSpanCtx};
use rle::zip::{rle_zip, rle_zip3};
use crate::causalgraph::agent_span::AgentSpan;
use crate::causalgraph::entry::CGEntry;
use crate::causalgraph::graph::GraphEntrySimple;
use crate::list::op_metrics::{ListOperationCtx, ListOpMetrics};
use crate::list::ListOpLog;
use crate::list::operation::TextOperation;
use crate::dtrange::DTRange;
use crate::rle::{KVPair, RleKeyedAndSplitable, RleSpanHelpers, RleVec};
use crate::{AgentId, Frontier, LV};

#[derive(Debug)]
pub(crate) struct OpMetricsIter<'a> {
    list: &'a RleVec<KVPair<ListOpMetrics>>,

    // I'd really like to take the ctx out of this structure. Right now this is very text-specific!
    //
    // To use this code with non-text, we need to remove this. But thats not so easy! I could make
    // it a generic parameter, but we'd end up monomorphizing a huge amount of code if this
    // structure became generic on the list type.
    //
    // This is needed here because we need to reference the op context to split operations, since
    // the operation metrics contain character (byte) offsets.
    pub(crate) ctx: &'a ListOperationCtx,

    /// The input span we're processing.
    range: DTRange,
    /// Current index
    idx: usize,
}

/// Wrapper around OpMetricsIter which yields (metrics, content) instead of just metrics.
#[derive(Debug)]
pub(crate) struct OpMetricsWithContent<'a>(OpMetricsIter<'a>);

impl<'a> From<OpMetricsIter<'a>> for OpMetricsWithContent<'a> {
    fn from(inner: OpMetricsIter<'a>) -> Self {
        OpMetricsWithContent(inner)
    }
}

impl<'a> Iterator for OpMetricsIter<'a> {
    type Item = KVPair<ListOpMetrics>;

    fn next(&mut self) -> Option<Self::Item> {
        // I bet there's a more efficient way to write this function.
        if self.idx >= self.list.0.len() { return None; }

        let KVPair(mut time, mut c) = self.list[self.idx].clone();
        if time >= self.range.end { return None; }

        if time + c.len() > self.range.end {
            c.truncate_ctx(self.range.end - time, self.ctx);
        }

        if time < self.range.start {
            c.truncate_keeping_right_ctx(self.range.start - time, self.ctx);
            time = self.range.start;
        }

        self.idx += 1;
        Some(KVPair(time, c))
    }
}

impl<'a> Iterator for OpMetricsWithContent<'a> {
    type Item = (KVPair<ListOpMetrics>, Option<&'a str>);

    fn next(&mut self) -> Option<Self::Item> {
        let metrics = self.0.next()?;
        let content = self.0.get_content(&metrics);
        Some((metrics, content))
    }
}

impl<'a> OpMetricsIter<'a> {
    pub(crate) fn new(list: &'a RleVec<KVPair<ListOpMetrics>>, ctx: &'a ListOperationCtx, range: DTRange) -> Self {
        let mut iter = OpMetricsIter {
            list,
            ctx,
            idx: 0,
            range
        };
        iter.prime(range);
        iter
    }

    fn prime(&mut self, range: DTRange) {
        self.range = range;
        self.idx = if range.is_empty() { 0 } else { self.list.find_next_index(range.start) };
    }

    #[allow(unused)]
    pub(crate) fn is_empty(&self) -> bool {
        self.idx >= self.list.0.len() || self.range.is_empty()
    }

    pub(crate) fn get_content(&self, metrics: &KVPair<ListOpMetrics>) -> Option<&'a str> {
        metrics.1.content_pos.map(|pos| {
            self.ctx.get_str(metrics.1.kind, pos)
        })
    }
}

impl<'a> OpMetricsWithContent<'a> {
    fn new(oplog: &'a ListOpLog, range: DTRange) -> Self {
        Self(OpMetricsIter::new(&oplog.operations, &oplog.operation_ctx, range))
    }
}

/// This is a variant on OpMetricsWithContent which yields operations since some (complex) point in
/// time in a document.
#[derive(Debug)]
struct OpIterRanges<'a> {
    ranges_rev: SmallVec<[DTRange; 4]>, // We own this. This is in descending order.
    current: OpMetricsWithContent<'a>
}

impl<'a> OpIterRanges<'a> {
    fn new(oplog: &'a ListOpLog, mut ranges_rev: SmallVec<[DTRange; 4]>) -> Self {
        let last = ranges_rev.pop().unwrap_or_else(|| (0..0).into());
        Self {
            ranges_rev,
            current: OpMetricsWithContent::new(oplog, last)
        }
    }
}

impl<'a> Iterator for OpIterRanges<'a> {
    // type Item = KVPair<OperationInternal>;
    type Item = (KVPair<ListOpMetrics>, Option<&'a str>);

    fn next(&mut self) -> Option<Self::Item> {
        let inner_next = self.current.next();
        if inner_next.is_some() { return inner_next; }

        if let Some(range) = self.ranges_rev.pop() {
            debug_assert!(!range.is_empty());
            self.current.0.prime(range);
            let inner_next = self.current.next();
            if inner_next.is_some() { return inner_next; }
        }

        None
    }
}

impl ListOpLog {
    // TODO: Consider removing these functions if they're never used.
    #[allow(unused)]
    pub(crate) fn iter_metrics_range(&self, range: DTRange) -> OpMetricsIter {
        OpMetricsIter::new(&self.operations, &self.operation_ctx, range)
    }

    #[allow(unused)]
    pub(crate) fn iter_metrics(&self) -> OpMetricsIter {
        self.iter_metrics_range((0..self.len()).into())
    }

    pub(crate) fn iter_range_simple(&self, range: DTRange) -> OpMetricsWithContent {
        OpMetricsWithContent::new(self, range)
    }

    pub fn iter_range_since(&self, local_version: &[LV]) -> impl Iterator<Item=TextOperation> + '_ {
        let only_b = self.cg.diff_since_rev(local_version);

        OpIterRanges::new(self, only_b)
            .map(|pair| (pair.0.1, pair.1).into())
    }

    pub(crate) fn iter_fast(&self) -> OpMetricsWithContent {
        OpMetricsWithContent::new(self, (0..self.len()).into())
    }

    pub fn iter(&self) -> impl Iterator<Item=TextOperation> + '_ {
        self.iter_fast().map(|pair| (pair.0.1, pair.1).into())
    }
}


#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FullEntry {
    pub span: DTRange,
    pub parents: Frontier,
    pub agent_span: AgentSpan,
    pub ops: SmallVec<[TextOperation; 2]>,
}

impl ListOpLog {
    pub fn iter_full<'a>(&'a self, simple_graph: &'a RleVec<GraphEntrySimple>) -> impl Iterator<Item = (GraphEntrySimple, AgentSpan, TextOperation)> + 'a {
        self.iter_fast().flat_map(|(pair, content)| {
            let range = pair.range();
            let simple_splits = simple_graph.iter_range(range);
            let aa = self.cg.agent_assignment.client_with_localtime.iter_range(range)
                .map(|KVPair(_, data)| data);

            let op: TextOperation = (pair.1, content).into();

            rle_zip3(simple_splits, aa, std::iter::once(op))
        })
    }

    /// This is a variant on iter_full, but where we also group together operations which are
    /// consecutive (from the same agent, and consecutive in time).
    ///
    /// TODO: Convert this to return an iterator.
    pub fn as_chunked_operation_vec(&self) -> Vec<FullEntry> {
        let mut result = vec![];
        let simple_graph = self.cg.make_simple_graph();

        for mut entry in simple_graph.0.into_iter() {
            for agent_kv in self.cg.agent_assignment.client_with_localtime.iter_range(entry.span) {
                let entry_here = entry.truncate_keeping_right_from(agent_kv.end());

                assert_eq!(agent_kv.range(), entry_here.span);

                result.push(FullEntry {
                    agent_span: agent_kv.1,
                    span: entry_here.span,
                    parents: entry_here.parents,
                    ops: self.iter_range_simple(entry_here.span)
                        .map(|pair| (pair.0.1, pair.1).into())
                        .collect(),
                });
            }
        }

        result
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::list::operation::ListOpKind;
    use crate::rle::{KVPair, RleVec};
    use ListOpKind::*;

    #[test]
    fn iter_smoke() {
        let mut ops: RleVec<KVPair<ListOpMetrics>> = RleVec::new();

        ops.push(KVPair(0, ListOpMetrics {
            loc: (100..110).into(),
            kind: Ins,
            content_pos: Some((0..10).into()),
        }));
        ops.push(KVPair(10, ListOpMetrics {
            loc: (200..220).into(),
            kind: Del,
            content_pos: None,
        }));

        let ctx = ListOperationCtx {
            ins_content: "0123456789".to_string().into_bytes(),
            del_content: "".to_string().into_bytes()
        };

        assert_eq!(OpMetricsIter::new(&ops, &ctx, (0..30).into()).collect::<Vec<_>>(), ops.0.as_slice());
        
        assert_eq!(OpMetricsIter::new(&ops, &ctx, (1..5).into()).collect::<Vec<_>>(), &[KVPair(1, ListOpMetrics {
            loc: (101..105).into(),
            kind: Ins,
            content_pos: Some((1..5).into()),
        })]);

        assert_eq!(OpMetricsIter::new(&ops, &ctx, (6..16).into()).collect::<Vec<_>>(), &[
            KVPair(6, ListOpMetrics {
                loc: (106..110).into(),
                kind: Ins,
                content_pos: Some((6..10).into()),
            }),
            KVPair(10, ListOpMetrics {
                loc: (200..206).into(),
                kind: Del,
                content_pos: None,
            }),
        ]);
    }

    // #[test]
    // #[ignore]
    // fn test_file() {
    //     use crate::list::encoding::{ENCODE_FULL, EncodeOptions};
    //     let data = std::fs::read("friendsforever.dt").unwrap();
    //     let oplog = ListOpLog::load_from(&data).unwrap();
    //     // oplog.checkout_tip();
    //
    //     let mut chunks = oplog.as_chunked_operation_vec();
    //     for (i, c) in chunks[..10].iter().enumerate() {
    //         println!("{i}: {:?}", c);
    //     }
    //
    //     chunks[5].parents.replace_with_1(56);
    //     chunks[6].parents.replace_with_1(119);
    //
    //     // Now form it back into an oplog.
    //
    //     let agent_names = [
    //         "0",
    //         "1",
    //         "0",
    //         "1",
    //     ];
    //
    //     let mut result = ListOpLog::new();
    //     for c in chunks {
    //         // let agent_name = oplog.get_agent_name(c.agent_span.agent);
    //         let agent_name = &agent_names[c.agent_span.agent as usize];
    //         let a = result.get_or_create_agent_id(agent_name);
    //
    //         result.add_operations_at(a, c.parents.as_ref(), &c.ops);
    //     }
    //
    //     let r1 = oplog.checkout_tip();
    //     let r2 = result.checkout_tip();
    //     assert_eq!(r1.content, r2.content);
    //
    //     dbg!(oplog.encode(ENCODE_FULL).len());
    //     dbg!(result.encode(ENCODE_FULL).len());
    //     let result_data = result.encode(ENCODE_FULL);
    //     std::fs::write("ff2.dt", &result_data).unwrap();
    // }
}