//! Test macros for vx that don't depend on syn
//!
//! Provides #[vx_test] as a replacement for #[tokio::test] without using tokio-macros

use proc_macro::TokenStream;
use quote::quote;

// Convert between proc_macro and proc_macro2 types
fn to_proc_macro2(stream: TokenStream) -> proc_macro2::TokenStream {
    stream.into()
}

fn from_proc_macro2(stream: proc_macro2::TokenStream) -> TokenStream {
    stream.into()
}

/// Attribute macro that wraps async test functions with a tokio runtime
///
/// This replaces #[tokio::test] without depending on tokio-macros (which pulls in syn).
/// Each test creates its own single-threaded runtime.
///
/// # Example
///
/// ```ignore
/// #[vx_test]
/// async fn my_test() {
///     assert_eq!(2 + 2, 4);
/// }
/// ```
#[proc_macro_attribute]
pub fn vx_test(_attr: TokenStream, item: TokenStream) -> TokenStream {
    // Convert to Vec<TokenTree> for simple matching
    let tokens: Vec<proc_macro::TokenTree> = item.into_iter().collect();

    // Find the function name - look for 'fn' keyword, then get next identifier
    let fn_pos = tokens
        .iter()
        .position(|tt| matches!(tt, proc_macro::TokenTree::Ident(id) if id.to_string() == "fn"));

    let fn_pos = match fn_pos {
        Some(pos) => pos,
        None => {
            return quote! {
                compile_error!("vx_test can only be used on functions");
            }
            .into();
        }
    };

    // Get function name (next ident after 'fn') - convert to proc_macro2
    let fn_name_pm1 = match tokens.get(fn_pos + 1) {
        Some(proc_macro::TokenTree::Ident(name)) => name.clone(),
        _ => {
            return quote! {
                compile_error!("Expected function name after 'fn'");
            }
            .into();
        }
    };
    let fn_name = proc_macro2::Ident::new(&fn_name_pm1.to_string(), proc_macro2::Span::call_site());

    // Find the function body (the last Group token should be the body)
    let body_idx = tokens
        .iter()
        .rposition(|tt| matches!(tt, proc_macro::TokenTree::Group(_)));

    let body = match body_idx {
        Some(idx) => {
            if let proc_macro::TokenTree::Group(g) = &tokens[idx] {
                to_proc_macro2(g.stream())
            } else {
                return quote! {
                    compile_error!("Expected function body");
                }
                .into();
            }
        }
        None => {
            return quote! {
                compile_error!("Expected function body");
            }
            .into();
        }
    };

    // Generate the wrapper
    let output = quote! {
        #[test]
        fn #fn_name() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                #body
            });
        }
    };

    from_proc_macro2(output)
}
