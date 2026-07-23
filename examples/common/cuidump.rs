//! Decode every named image in a .car via Apple's private CUICatalog and write premultiplied
//! "RGBA"-magic dumps as <outdir>/<safe_name>__<w>x<h>@<scale>x__<idx>.rgbaref. macOS only.

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CStr, CString, c_char, c_int, c_void};
    use std::path::Path;

    use objc2::rc::autoreleasepool;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CGPoint {
        pub x: f64,
        pub y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CGSize {
        pub width: f64,
        pub height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CGRect {
        pub origin: CGPoint,
        pub size: CGSize,
    }
    // Struct return through msg_send! needs an objc type encoding.
    unsafe impl objc2::Encode for CGSize {
        const ENCODING: objc2::Encoding = objc2::Encoding::Struct(
            "CGSize",
            &[objc2::Encoding::Double, objc2::Encoding::Double],
        );
    }
    /// Opaque CGImage, encoded as `^{CGImage=}` so objc2's debug-build verification accepts `-[CUINamedImage image]`.
    #[repr(C)]
    pub struct CGImage {
        _opaque: [u8; 0],
    }
    unsafe impl objc2::RefEncode for CGImage {
        const ENCODING_REF: objc2::Encoding =
            objc2::Encoding::Pointer(&objc2::Encoding::Struct("CGImage", &[]));
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGImageGetWidth(img: *mut c_void) -> usize;
        fn CGImageGetHeight(img: *mut c_void) -> usize;
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            colorspace: *mut c_void,
            bitmap_info: u32,
        ) -> *mut c_void;
        fn CGContextDrawImage(ctx: *mut c_void, rect: CGRect, img: *mut c_void);
        fn CGContextRelease(ctx: *mut c_void);
        fn CGColorSpaceRelease(cs: *mut c_void);
    }
    // Force Foundation to load so NSString/NSURL classes resolve.
    #[link(name = "Foundation", kind = "framework")]
    unsafe extern "C" {}
    unsafe extern "C" {
        fn dlopen(path: *const c_char, flag: c_int) -> *mut c_void;
    }

    const ALPHA_PREMULTIPLIED_LAST: u32 = 1;
    const BYTE_ORDER_32_BIG: u32 = 4 << 12;
    const RTLD_NOW: c_int = 2;

    unsafe fn nsstring(s: &str) -> *mut AnyObject {
        let c = CString::new(s).unwrap();
        msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()]
    }

    /// CGImage -> raw "RGBA" dump (magic, u32 w, u32 h, premultiplied rows).
    unsafe fn dump_rgba(img: *mut c_void, outpath: &Path) {
        unsafe {
            let (w, h) = (CGImageGetWidth(img), CGImageGetHeight(img));
            let bpr = w * 4;
            let mut buf = vec![0u8; bpr * h];
            let cs = CGColorSpaceCreateDeviceRGB();
            let ctx = CGBitmapContextCreate(
                buf.as_mut_ptr().cast(),
                w,
                h,
                8,
                bpr,
                cs,
                ALPHA_PREMULTIPLIED_LAST | BYTE_ORDER_32_BIG,
            );
            let rect = CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: w as f64,
                    height: h as f64,
                },
            };
            CGContextDrawImage(ctx, rect, img);
            CGContextRelease(ctx);
            CGColorSpaceRelease(cs);

            let mut out = Vec::with_capacity(12 + buf.len());
            out.extend_from_slice(b"RGBA");
            out.extend_from_slice(&(w as u32).to_le_bytes());
            out.extend_from_slice(&(h as u32).to_le_bytes());
            out.extend_from_slice(&buf);
            std::fs::write(outpath, out).expect("writing rgbaref");
        }
    }

    /// `%g`-style scale formatting ("1", "2", "1.5"), matching the C version.
    fn fmt_scale(s: f64) -> String {
        if s == s.trunc() {
            format!("{}", s as i64)
        } else {
            format!("{s}")
        }
    }

    pub fn dump(car: &Path, outdir: &Path, filter: Option<&str>) -> usize {
        unsafe {
            let coreui =
                CString::new("/System/Library/PrivateFrameworks/CoreUI.framework/CoreUI").unwrap();
            assert!(
                !dlopen(coreui.as_ptr(), RTLD_NOW).is_null(),
                "dlopen CoreUI failed"
            );
            std::fs::create_dir_all(outdir).expect("creating outdir");

            let url: *mut AnyObject = msg_send![
                class!(NSURL),
                fileURLWithPath: nsstring(car.to_str().expect("catalog path must be UTF-8"))
            ];
            let mut err: *mut AnyObject = std::ptr::null_mut();
            let cat: *mut AnyObject = msg_send![class!(CUICatalog), alloc];
            let cat: *mut AnyObject = msg_send![cat, initWithURL: url, error: &mut err];
            assert!(
                !cat.is_null(),
                "CUICatalog initWithURL failed for {}",
                car.display()
            );

            let names: *mut AnyObject = msg_send![cat, allImageNames];
            let count: usize = msg_send![names, count];
            let mut dumped = 0usize;
            for i in 0..count {
                autoreleasepool(|_| {
                    let name_obj: *mut AnyObject = msg_send![names, objectAtIndex: i];
                    let name_c: *const c_char = msg_send![name_obj, UTF8String];
                    let name = CStr::from_ptr(name_c).to_string_lossy().into_owned();
                    if let Some(f) = filter {
                        if !name.contains(f) {
                            return;
                        }
                    }
                    let safe: String = name
                        .chars()
                        .map(|c| if c.is_alphanumeric() { c } else { '_' })
                        .collect();

                    let imgs: *mut AnyObject = msg_send![cat, imagesWithName: name_obj];
                    let n_imgs: usize = msg_send![imgs, count];
                    for idx in 0..n_imgs {
                        let ni: *mut AnyObject = msg_send![imgs, objectAtIndex: idx];
                        // Skip non-image objects explicitly: objc2's debug-build send verification panics in Rust, so exception::catch cannot cover a missing `image` selector.
                        let has_image: bool = msg_send![ni, respondsToSelector: objc2::sel!(image)];
                        if !has_image {
                            continue;
                        }
                        // [ni image] can throw inside CoreUI on undecodable renditions.
                        let cg = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                            let cg: *mut CGImage = msg_send![ni, image];
                            cg
                        }));
                        let Ok(cg) = cg else { continue };
                        if cg.is_null() {
                            continue;
                        }
                        let cg: *mut c_void = cg.cast();
                        let size: CGSize = msg_send![ni, size];
                        let scale: f64 = msg_send![ni, scale];
                        let out = outdir.join(format!(
                            "{safe}__{}x{}@{}x__{idx}.rgbaref",
                            size.width as i32,
                            size.height as i32,
                            fmt_scale(scale)
                        ));
                        dump_rgba(cg, &out);
                        dumped += 1;
                    }
                });
            }
            dumped
        }
    }
}

#[cfg(target_os = "macos")]
pub use imp::dump;

#[cfg(not(target_os = "macos"))]
pub fn dump(_car: &std::path::Path, _outdir: &std::path::Path, _filter: Option<&str>) -> usize {
    panic!("cuidump requires macOS (private CoreUI framework)");
}
