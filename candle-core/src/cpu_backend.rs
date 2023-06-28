use crate::op::{BinaryOp, UnaryOp};
use crate::{DType, Error, Layout, Result, Shape, WithDType};
use gemm::{gemm, Parallelism};
use half::{bf16, f16};

// TODO: Maybe we should not implement [Clone] here and instead have an explicit allocator +
// intercept the oom errors to avoid panicking and provide a proper error.
#[derive(Debug, Clone)]
pub enum CpuStorage {
    U32(Vec<u32>),
    BF16(Vec<bf16>),
    F16(Vec<f16>),
    F32(Vec<f32>),
    F64(Vec<f64>),
}

trait Map1 {
    fn f<T: WithDType + Copy + num_traits::NumAssign>(
        &self,
        vs: &[T],
        layout: &Layout,
    ) -> Result<Vec<T>>;

    fn map(&self, vs: &CpuStorage, layout: &Layout) -> Result<CpuStorage> {
        match vs {
            CpuStorage::U32(vs) => Ok(CpuStorage::U32(self.f(vs, layout)?)),
            CpuStorage::BF16(vs) => Ok(CpuStorage::BF16(self.f(vs, layout)?)),
            CpuStorage::F16(vs) => Ok(CpuStorage::F16(self.f(vs, layout)?)),
            CpuStorage::F32(vs) => Ok(CpuStorage::F32(self.f(vs, layout)?)),
            CpuStorage::F64(vs) => Ok(CpuStorage::F64(self.f(vs, layout)?)),
        }
    }
}

fn wcond<T: Copy>(
    pred: &[u32],
    layout: &Layout,
    t: &[T],
    layout_t: &Layout,
    f: &[T],
    layout_f: &Layout,
) -> Vec<T> {
    match (
        layout.contiguous_offsets(),
        layout_t.contiguous_offsets(),
        layout_f.contiguous_offsets(),
    ) {
        (Some((o1, o2)), Some((o_t1, o_t2)), Some((o_f1, o_f2))) => {
            let pred = &pred[o1..o2];
            let t = &t[o_t1..o_t2];
            let f = &f[o_f1..o_f2];
            pred.iter()
                .zip(t.iter().zip(f.iter()))
                .map(|(&p, (&t, &f))| if p > 0 { t } else { f })
                .collect::<Vec<_>>()
        }
        _ => layout
            .strided_index()
            .zip(layout_t.strided_index().zip(layout_f.strided_index()))
            .map(|(i_p, (i_t, i_f))| if pred[i_p] > 0 { t[i_t] } else { f[i_f] })
            .collect::<Vec<_>>(),
    }
}

struct Sum<'a> {
    dst_shape: &'a Shape,
    sum_dims_and_stride: Vec<(usize, usize)>,
}

impl<'a> Map1 for Sum<'a> {
    fn f<T: WithDType + Copy + num_traits::NumAssign>(
        &self,
        src: &[T],
        src_layout: &Layout,
    ) -> Result<Vec<T>> {
        let mut dst = vec![T::zero(); self.dst_shape.elem_count()];
        for (unstr_index, src_index) in src_layout.strided_index().enumerate() {
            let mut dst_index = unstr_index;
            // Set the sum_dims indexes to 0.
            for &(dim, stride) in self.sum_dims_and_stride.iter() {
                // The compiler is able to optimize the following in a single divmod op.
                let (pre, post) = (dst_index / stride, dst_index % stride);
                dst_index = (pre / dim) * stride + post;
            }
            dst[dst_index] += src[src_index];
        }
        Ok(dst)
    }
}

fn unary_map<T: Copy, U: Copy, F: FnMut(T) -> U>(vs: &[T], layout: &Layout, mut f: F) -> Vec<U> {
    match layout.contiguous_offsets() {
        Some((o1, o2)) => vs[o1..o2].iter().map(|&v| f(v)).collect(),
        None => layout.strided_index().map(|i| f(vs[i])).collect(),
    }
}

// This function maps over two strided index sequences.
fn binary_map<T: Copy, F: FnMut(T, T) -> T>(
    lhs_l: &Layout,
    rhs_l: &Layout,
    lhs: &[T],
    rhs: &[T],
    mut f: F,
) -> Vec<T> {
    match (lhs_l.contiguous_offsets(), rhs_l.contiguous_offsets()) {
        (Some((o_l1, o_l2)), Some((o_r1, o_r2))) => lhs[o_l1..o_l2]
            .iter()
            .zip(rhs[o_r1..o_r2].iter())
            .map(|(&l, &r)| f(l, r))
            .collect(),
        _ => lhs_l
            .strided_index()
            .zip(rhs_l.strided_index())
            .map(|(lhs_i, rhs_i)| f(lhs[lhs_i], rhs[rhs_i]))
            .collect(),
    }
}

struct Affine(f64, f64);

impl Map1 for Affine {
    fn f<T: WithDType + Copy + num_traits::NumAssign>(
        &self,
        vs: &[T],
        layout: &Layout,
    ) -> Result<Vec<T>> {
        let mul = T::from_f64(self.0);
        let add = T::from_f64(self.1);
        Ok(unary_map(vs, layout, |v| v * mul + add))
    }
}

struct Embedding<'a> {
    vocab_size: usize,
    hidden_size: usize,
    ids: &'a [u32],
    ids_l: &'a Layout,
}

impl<'a> Map1 for Embedding<'a> {
    fn f<T: WithDType>(&self, vs: &[T], layout: &Layout) -> Result<Vec<T>> {
        // TODO: We assume that vs is contiguous here.
        let vs = &vs[layout.start_offset()..];
        let mut values = Vec::with_capacity(self.ids_l.shape().elem_count() * self.hidden_size);
        // TODO: Optimize for the case where ids are contiguous.
        for index in self.ids_l.strided_index() {
            let index = self.ids[index].try_into()?;
            if index >= self.vocab_size {
                return Err(Error::InvalidIndex {
                    index,
                    vocab_size: self.vocab_size,
                    op: "take",
                });
            } else {
                let hidden_size = self.hidden_size;
                values.extend(&vs[hidden_size * index..hidden_size * (index + 1)]);
            }
        }
        Ok(values)
    }
}

fn copy_strided_src_<T: Copy + std::fmt::Display>(
    src: &[T],
    dst: &mut [T],
    dst_offset: usize,
    src_l: &Layout,
) {
    match src_l.contiguous_offsets() {
        Some((o_dst1, o_dst2)) => {
            let elem_to_copy = (dst.len() - dst_offset).min(o_dst2 - o_dst1);
            dst[dst_offset..dst_offset + elem_to_copy].copy_from_slice(&src[o_dst1..o_dst2])
        }
        None => {
            for (dst_index, src_index) in src_l.strided_index().enumerate() {
                let dst_index = dst_index + dst_offset;
                if dst_index >= dst.len() {
                    break;
                }
                dst[dst_index] = src[src_index]
            }
        }
    }
}

fn matmul<T: 'static + num_traits::Num + Copy>(
    lhs: &[T],
    rhs: &[T],
    (b, m, n, k): (usize, usize, usize, usize),
    lhs_l: &Layout,
    rhs_l: &Layout,
) -> Result<Vec<T>> {
    let lhs = &lhs[lhs_l.start_offset()..];
    let rhs = &rhs[rhs_l.start_offset()..];
    let a_skip: usize = m * k;
    let b_skip: usize = n * k;
    let c_skip: usize = m * n;

    let lhs_stride = lhs_l.stride();
    let rhs_stride = rhs_l.stride();
    let rank = lhs_stride.len();
    let lhs_cs = lhs_stride[rank - 1];
    let lhs_rs = lhs_stride[rank - 2];

    let rhs_cs = rhs_stride[rank - 1];
    let rhs_rs = rhs_stride[rank - 2];

    if lhs_stride.len() > 2 {
        let lhs_batch_stride = &lhs_stride[..rank - 2];
        let rhs_batch_stride = &rhs_stride[..rank - 2];

        if lhs_batch_stride != [a_skip] || rhs_batch_stride != [b_skip] {
            // Temporary error before we support abitrary striding.
            return Err(Error::UnexpectedStriding);
        }
    }

    let dst_shape: Shape = (m, n).into();
    let dst_strides = dst_shape.stride_contiguous();
    let dst_rs = dst_strides[0];
    let dst_cs = dst_strides[1];

    let mut dst = vec![T::zero(); b * m * n];
    for step in 0..b {
        let lhs_p = &lhs[step * a_skip..];
        let rhs_p = &rhs[step * b_skip..];
        let dst_p = &mut dst[step * c_skip..];
        unsafe {
            gemm(
                /* m: usize = */ m,
                /* n: usize = */ n,
                /* k: usize = */ k,
                /* dst: *mut T = */ dst_p.as_mut_ptr(),
                /* dst_cs: isize = */ dst_cs as isize,
                /* dst_rs: isize = */ dst_rs as isize,
                /* read_dst: bool = */ false,
                /* lhs: *const T = */ lhs_p.as_ptr(),
                /* lhs_cs: isize = */ lhs_cs as isize,
                /* lhs_rs: isize = */ lhs_rs as isize,
                /* rhs: *const T = */ rhs_p.as_ptr(),
                /* rhs_cs: isize = */ rhs_cs as isize,
                /* rhs_rs: isize = */ rhs_rs as isize,
                /* alpha: T = */ T::zero(),
                /* beta: T = */ T::one(),
                /* conj_dst: bool = */ false,
                /* conj_lhs: bool = */ false,
                /* conj_rhs: bool = */ false,
                Parallelism::Rayon(crate::utils::get_num_threads()),
            )
        }
    }
    Ok(dst)
}

impl CpuStorage {
    pub fn dtype(&self) -> DType {
        match self {
            Self::U32(_) => DType::U32,
            Self::BF16(_) => DType::BF16,
            Self::F16(_) => DType::F16,
            Self::F32(_) => DType::F32,
            Self::F64(_) => DType::F64,
        }
    }

    pub fn as_slice<D: crate::WithDType>(&self) -> Result<&[D]> {
        D::cpu_storage_as_slice(self)
    }

    pub(crate) fn to_dtype(&self, layout: &Layout, dtype: DType) -> Result<Self> {
        // TODO: find a way around the quadratic number of cases below.
        match (self, dtype) {
            (Self::U32(storage), DType::BF16) => {
                let data = unary_map(storage, layout, |v| bf16::from_f32(v as f32));
                Ok(Self::BF16(data))
            }
            (Self::BF16(storage), DType::BF16) => {
                let data = unary_map(storage, layout, |v| v);
                Ok(Self::BF16(data))
            }
            (Self::F16(storage), DType::BF16) => {
                let data = unary_map(storage, layout, |v| bf16::from_f32(v.to_f32()));
                Ok(Self::BF16(data))
            }
            (Self::F32(storage), DType::BF16) => {
                let data = unary_map(storage, layout, bf16::from_f32);
                Ok(Self::BF16(data))
            }
            (Self::F64(storage), DType::BF16) => {
                let data = unary_map(storage, layout, bf16::from_f64);
                Ok(Self::BF16(data))
            }
            (Self::U32(storage), DType::F16) => {
                let data = unary_map(storage, layout, |v| f16::from_f32(v as f32));
                Ok(Self::F16(data))
            }
            (Self::BF16(storage), DType::F16) => {
                let data = unary_map(storage, layout, |v| f16::from_f32(v.to_f32()));
                Ok(Self::F16(data))
            }
            (Self::F16(storage), DType::F16) => {
                let data = unary_map(storage, layout, |v| v);
                Ok(Self::F16(data))
            }
            (Self::F32(storage), DType::F16) => {
                let data = unary_map(storage, layout, f16::from_f32);
                Ok(Self::F16(data))
            }
            (Self::F64(storage), DType::F16) => {
                let data = unary_map(storage, layout, f16::from_f64);
                Ok(Self::F16(data))
            }
            (Self::U32(storage), DType::F32) => {
                let data = unary_map(storage, layout, |v| v as f32);
                Ok(Self::F32(data))
            }
            (Self::BF16(storage), DType::F32) => {
                let data = unary_map(storage, layout, |v| v.to_f32());
                Ok(Self::F32(data))
            }
            (Self::F16(storage), DType::F32) => {
                let data = unary_map(storage, layout, |v| v.to_f32());
                Ok(Self::F32(data))
            }
            (Self::F32(storage), DType::F32) => {
                let data = unary_map(storage, layout, |v| v);
                Ok(Self::F32(data))
            }
            (Self::F64(storage), DType::F32) => {
                let data = unary_map(storage, layout, |v| v as f32);
                Ok(Self::F32(data))
            }
            (Self::U32(storage), DType::U32) => {
                let data = unary_map(storage, layout, |v| v);
                Ok(Self::U32(data))
            }
            (Self::BF16(storage), DType::U32) => {
                let data = unary_map(storage, layout, |v| v.to_f32() as u32);
                Ok(Self::U32(data))
            }
            (Self::F16(storage), DType::U32) => {
                let data = unary_map(storage, layout, |v| v.to_f32() as u32);
                Ok(Self::U32(data))
            }
            (Self::F32(storage), DType::U32) => {
                let data = unary_map(storage, layout, |v| v as u32);
                Ok(Self::U32(data))
            }
            (Self::F64(storage), DType::U32) => {
                let data = unary_map(storage, layout, |v| v as u32);
                Ok(Self::U32(data))
            }
            (Self::U32(storage), DType::F64) => {
                let data = unary_map(storage, layout, |v| v as f64);
                Ok(Self::F64(data))
            }
            (Self::BF16(storage), DType::F64) => {
                let data = unary_map(storage, layout, |v| v.to_f64());
                Ok(Self::F64(data))
            }
            (Self::F16(storage), DType::F64) => {
                let data = unary_map(storage, layout, |v| v.to_f64());
                Ok(Self::F64(data))
            }
            (Self::F32(storage), DType::F64) => {
                let data = unary_map(storage, layout, |v| v as f64);
                Ok(Self::F64(data))
            }
            (Self::F64(storage), DType::F64) => {
                let data = unary_map(storage, layout, |v| v);
                Ok(Self::F64(data))
            }
        }
    }

    pub(crate) fn sum(&self, layout: &Layout, sum_dims: &[usize]) -> Result<Self> {
        let src_dims = layout.dims();
        let mut dst_dims = src_dims.to_vec();
        for &sum_dim in sum_dims.iter() {
            dst_dims[sum_dim] = 1;
        }
        let dst_shape = Shape::from(dst_dims);
        let mut sum_dims = sum_dims.to_vec();
        // Sort the sum_dims as they have to be processed from left to right when converting the
        // indexes.
        sum_dims.sort();
        let sum_dims_and_stride: Vec<_> = sum_dims
            .iter()
            .map(|&d| (src_dims[d], src_dims[d + 1..].iter().product::<usize>()))
            .collect();
        Sum {
            dst_shape: &dst_shape,
            sum_dims_and_stride,
        }
        .map(self, layout)
    }

    pub(crate) fn divide_by_sum_over_dim(&mut self, shape: &Shape, dim: usize) -> Result<()> {
        // [self] stores data in a contiguous way starting at offset 0.
        let dims = shape.dims();
        let elem_per_slice = dims[dim];
        let prod_pre_dim = dims[..dim].iter().product();
        let prod_post_dim = dims[dim + 1..].iter().product();
        match self {
            Self::BF16(storage) => {
                for pre_idx in 0..prod_pre_dim {
                    for post_idx in 0..prod_post_dim {
                        let mut sum = 0f64;
                        let mut idx = pre_idx * prod_post_dim * elem_per_slice + post_idx;
                        for _ in 0..elem_per_slice {
                            sum += storage[idx].to_f64();
                            idx += prod_post_dim
                        }
                        let sum = bf16::from_f64(sum);
                        let mut idx = pre_idx * prod_post_dim * elem_per_slice + post_idx;
                        for _ in 0..elem_per_slice {
                            storage[idx] /= sum;
                            idx += prod_post_dim
                        }
                    }
                }
            }
            Self::F16(storage) => {
                for pre_idx in 0..prod_pre_dim {
                    for post_idx in 0..prod_post_dim {
                        let mut sum = 0f64;
                        let mut idx = pre_idx * prod_post_dim * elem_per_slice + post_idx;
                        for _ in 0..elem_per_slice {
                            sum += storage[idx].to_f64();
                            idx += prod_post_dim
                        }
                        let sum = f16::from_f64(sum);
                        let mut idx = pre_idx * prod_post_dim * elem_per_slice + post_idx;
                        for _ in 0..elem_per_slice {
                            storage[idx] /= sum;
                            idx += prod_post_dim
                        }
                    }
                }
            }
            Self::F32(storage) => {
                for pre_idx in 0..prod_pre_dim {
                    for post_idx in 0..prod_post_dim {
                        let mut sum = 0f64;
                        let mut idx = pre_idx * prod_post_dim * elem_per_slice + post_idx;
                        for _ in 0..elem_per_slice {
                            sum += storage[idx] as f64;
                            idx += prod_post_dim
                        }
                        let sum = sum as f32;
                        let mut idx = pre_idx * prod_post_dim * elem_per_slice + post_idx;
                        for _ in 0..elem_per_slice {
                            storage[idx] /= sum;
                            idx += prod_post_dim
                        }
                    }
                }
            }
            Self::F64(storage) => {
                for pre_idx in 0..prod_pre_dim {
                    for post_idx in 0..prod_post_dim {
                        let mut sum = 0f64;
                        let mut idx = pre_idx * prod_post_dim * elem_per_slice + post_idx;
                        for _ in 0..elem_per_slice {
                            sum += storage[idx];
                            idx += prod_post_dim
                        }
                        let mut idx = pre_idx * prod_post_dim * elem_per_slice + post_idx;
                        for _ in 0..elem_per_slice {
                            storage[idx] /= sum;
                            idx += prod_post_dim
                        }
                    }
                }
            }
            Self::U32(_) => {}
        }
        Ok(())
    }

    pub(crate) fn affine(&self, layout: &Layout, mul: f64, add: f64) -> Result<Self> {
        Affine(mul, add).map(self, layout)
    }

    pub(crate) fn unary_impl<B: UnaryOp>(&self, layout: &Layout) -> Result<Self> {
        match self {
            Self::BF16(storage) => {
                let data = unary_map(storage, layout, B::bf16);
                Ok(Self::BF16(data))
            }
            Self::F16(storage) => {
                let data = unary_map(storage, layout, B::f16);
                Ok(Self::F16(data))
            }
            Self::F32(storage) => {
                let data = unary_map(storage, layout, B::f32);
                Ok(Self::F32(data))
            }
            Self::F64(storage) => {
                let data = unary_map(storage, layout, B::f64);
                Ok(Self::F64(data))
            }
            Self::U32(storage) => {
                let data = unary_map(storage, layout, B::u32);
                Ok(Self::U32(data))
            }
        }
    }

    pub(crate) fn binary_impl<B: BinaryOp>(
        &self,
        rhs: &Self,
        lhs_l: &Layout,
        rhs_l: &Layout,
    ) -> Result<Self> {
        match (self, rhs) {
            (Self::BF16(lhs), Self::BF16(rhs)) => {
                let data = binary_map(lhs_l, rhs_l, lhs, rhs, B::bf16);
                Ok(Self::BF16(data))
            }
            (Self::F16(lhs), Self::F16(rhs)) => {
                let data = binary_map(lhs_l, rhs_l, lhs, rhs, B::f16);
                Ok(Self::F16(data))
            }
            (Self::F32(lhs), Self::F32(rhs)) => {
                let data = binary_map(lhs_l, rhs_l, lhs, rhs, B::f32);
                Ok(Self::F32(data))
            }
            (Self::F64(lhs), Self::F64(rhs)) => {
                let data = binary_map(lhs_l, rhs_l, lhs, rhs, B::f64);
                Ok(Self::F64(data))
            }
            (Self::U32(lhs), Self::U32(rhs)) => {
                let data = binary_map(lhs_l, rhs_l, lhs, rhs, B::u32);
                Ok(Self::U32(data))
            }
            _ => {
                // This should be covered by the dtype check above.
                Err(Error::DTypeMismatchBinaryOp {
                    lhs: self.dtype(),
                    rhs: rhs.dtype(),
                    op: B::NAME,
                })
            }
        }
    }

    pub(crate) fn copy_strided_src(
        &self,
        dst: &mut Self,
        dst_offset: usize,
        src_l: &Layout,
    ) -> Result<()> {
        match (self, dst) {
            (Self::U32(src), Self::U32(dst)) => copy_strided_src_(src, dst, dst_offset, src_l),
            (Self::BF16(src), Self::BF16(dst)) => copy_strided_src_(src, dst, dst_offset, src_l),
            (Self::F16(src), Self::F16(dst)) => copy_strided_src_(src, dst, dst_offset, src_l),
            (Self::F32(src), Self::F32(dst)) => copy_strided_src_(src, dst, dst_offset, src_l),
            (Self::F64(src), Self::F64(dst)) => copy_strided_src_(src, dst, dst_offset, src_l),
            (_, dst) => {
                // This should be covered by the dtype check above.
                return Err(Error::DTypeMismatchBinaryOp {
                    lhs: self.dtype(),
                    rhs: dst.dtype(),
                    op: "copy_strided",
                });
            }
        }
        Ok(())
    }

    pub(crate) fn where_cond(
        &self,
        layout: &Layout,
        t: &Self,
        layout_t: &Layout,
        f: &Self,
        layout_f: &Layout,
    ) -> Result<Self> {
        // TODO: Support types that could be casted to a boolean.
        let pred = self.as_slice::<u32>()?;
        match (t, f) {
            (Self::BF16(t), Self::BF16(f)) => {
                let data = wcond(pred, layout, t, layout_t, f, layout_f);
                Ok(Self::BF16(data))
            }
            (Self::F16(t), Self::F16(f)) => {
                let data = wcond(pred, layout, t, layout_t, f, layout_f);
                Ok(Self::F16(data))
            }
            (Self::F32(t), Self::F32(f)) => {
                let data = wcond(pred, layout, t, layout_t, f, layout_f);
                Ok(Self::F32(data))
            }
            (Self::F64(t), Self::F64(f)) => {
                let data = wcond(pred, layout, t, layout_t, f, layout_f);
                Ok(Self::F64(data))
            }
            (Self::U32(t), Self::U32(f)) => {
                let data = wcond(pred, layout, t, layout_t, f, layout_f);
                Ok(Self::U32(data))
            }
            _ => Err(Error::DTypeMismatchBinaryOp {
                lhs: t.dtype(),
                rhs: f.dtype(),
                op: "where_cond",
            }),
        }
    }

    pub(crate) fn embedding(&self, ids_l: &Layout, rhs: &Self, rhs_l: &Layout) -> Result<Self> {
        let ids = self.as_slice::<u32>()?;
        let (vocab_size, hidden_size) = rhs_l.shape().r2()?;
        Embedding {
            vocab_size,
            hidden_size,
            ids,
            ids_l,
        }
        .map(rhs, rhs_l)
    }

    pub(crate) fn matmul(
        &self,
        rhs: &Self,
        bmnk: (usize, usize, usize, usize),
        lhs_l: &Layout,
        rhs_l: &Layout,
    ) -> Result<Self> {
        match (self, rhs) {
            (CpuStorage::F16(lhs), CpuStorage::F16(rhs)) => {
                let dst = matmul(lhs, rhs, bmnk, lhs_l, rhs_l)?;
                Ok(Self::F16(dst))
            }
            (CpuStorage::F32(lhs), CpuStorage::F32(rhs)) => {
                let dst = matmul(lhs, rhs, bmnk, lhs_l, rhs_l)?;
                Ok(Self::F32(dst))
            }
            (CpuStorage::F64(lhs), CpuStorage::F64(rhs)) => {
                let dst = matmul(lhs, rhs, bmnk, lhs_l, rhs_l)?;
                Ok(Self::F64(dst))
            }
            _ => Err(Error::DTypeMismatchBinaryOp {
                lhs: self.dtype(),
                rhs: rhs.dtype(),
                op: "matmul",
            }),
        }
    }

    pub(crate) fn ones_impl(shape: &Shape, dtype: DType) -> Self {
        let elem_count = shape.elem_count();
        match dtype {
            DType::U32 => {
                let data = vec![1u32; elem_count];
                Self::U32(data)
            }
            DType::BF16 => {
                let data = vec![bf16::ONE; elem_count];
                Self::BF16(data)
            }
            DType::F16 => {
                let data = vec![f16::ONE; elem_count];
                Self::F16(data)
            }
            DType::F32 => {
                let data = vec![1f32; elem_count];
                Self::F32(data)
            }
            DType::F64 => {
                let data = vec![1f64; elem_count];
                Self::F64(data)
            }
        }
    }

    pub(crate) fn zeros_impl(shape: &Shape, dtype: DType) -> Self {
        let elem_count = shape.elem_count();
        match dtype {
            DType::U32 => {
                let data = vec![0u32; elem_count];
                Self::U32(data)
            }
            DType::BF16 => {
                let data = vec![bf16::ZERO; elem_count];
                Self::BF16(data)
            }
            DType::F16 => {
                let data = vec![f16::ZERO; elem_count];
                Self::F16(data)
            }
            DType::F32 => {
                let data = vec![0f32; elem_count];
                Self::F32(data)
            }
            DType::F64 => {
                let data = vec![0f64; elem_count];
                Self::F64(data)
            }
        }
    }
}
