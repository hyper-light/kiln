use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, DeriveInput};

pub fn derive_vtab_module(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);
    let struct_name = &ast.ident;

    let register_fn_name = format_ident!("register_{}", struct_name);
    let create_fn_name = format_ident!("create_{}", struct_name);
    let open_fn_name = format_ident!("open_{}", struct_name);
    let close_fn_name = format_ident!("close_{}", struct_name);
    let filter_fn_name = format_ident!("filter_{}", struct_name);
    let column_fn_name = format_ident!("column_{}", struct_name);
    let next_fn_name = format_ident!("next_{}", struct_name);
    let eof_fn_name = format_ident!("eof_{}", struct_name);
    let update_fn_name = format_ident!("update_{}", struct_name);
    let rowid_fn_name = format_ident!("rowid_{}", struct_name);
    let destroy_fn_name = format_ident!("destroy_{}", struct_name);
    let best_idx_fn_name = format_ident!("best_idx_{}", struct_name);
    let begin_fn_name = format_ident!("begin_{}", struct_name);
    let rollback_fn_name = format_ident!("rollback_{}", struct_name);
    let commit_fn_name = format_ident!("commit_{}", struct_name);
    let rename_fn_name = format_ident!("rename_{}", struct_name);

    let expanded = quote! {
        impl #struct_name {
            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            unsafe extern "C" fn #create_fn_name(
                argv: *const ::amphora_engine_ext::Value, argc: i32
            ) -> ::amphora_engine_ext::VTabCreateResult {
                let args = if argv.is_null() {
                    &Vec::new()
                } else {
                    ::std::slice::from_raw_parts(argv, argc as usize)
                };
                match <#struct_name as ::amphora_engine_ext::VTabModule>::create(&args) {
                    Ok((schema, table)) => {
                        ::amphora_engine_ext::VTabCreateResult {
                            code: ::amphora_engine_ext::ResultCode::OK,
                            schema: ::std::ffi::CString::new(schema).unwrap().into_raw(),
                            table: ::std::boxed::Box::into_raw(::std::boxed::Box::new(table)) as *const ::std::ffi::c_void,
                        }
                    },
                    Err(e) => {
                        ::amphora_engine_ext::VTabCreateResult {
                            code: e,
                            schema: ::std::ptr::null(),
                            table: ::std::ptr::null(),
                        }
                    }
                }
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            unsafe extern "C" fn #open_fn_name(table: *const ::std::ffi::c_void, conn: *const ::amphora_engine_ext::Conn) -> *const ::std::ffi::c_void {
                if table.is_null() {
                    return ::std::ptr::null();
                }
                let table = table as *const <#struct_name as ::amphora_engine_ext::VTabModule>::Table;
                let table: &<#struct_name as ::amphora_engine_ext::VTabModule>::Table = &*table;
                let conn = if conn.is_null() { None } else { Some(::std::sync::Arc::new(::amphora_engine_ext::Connection::new(conn)))};
                if let Ok(cursor) = <#struct_name as ::amphora_engine_ext::VTabModule>::Table::open(table, conn) {
                    return ::std::boxed::Box::into_raw(::std::boxed::Box::new(cursor)) as *const ::std::ffi::c_void;
                } else {
                    return ::std::ptr::null();
                }
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            unsafe extern "C" fn #close_fn_name(
                cursor: *const ::std::ffi::c_void
            ) -> ::amphora_engine_ext::ResultCode {
                if cursor.is_null() {
                    return ::amphora_engine_ext::ResultCode::Error;
                }
                let cursor = cursor as *mut <<#struct_name as ::amphora_engine_ext::VTabModule>::Table as ::amphora_engine_ext::VTable>::Cursor;
                let cursor = ::std::boxed::Box::from_raw(cursor);
                <<#struct_name as ::amphora_engine_ext::VTabModule>::Table as ::amphora_engine_ext::VTable>::Cursor::close(&*cursor)
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            unsafe extern "C" fn #filter_fn_name(
                cursor: *const ::std::ffi::c_void,
                argc: i32,
                argv: *const ::amphora_engine_ext::Value,
                idx_str: *const ::std::ffi::c_char,
                idx_num: i32,
            ) -> ::amphora_engine_ext::ResultCode {
                if cursor.is_null() {
                    return ::amphora_engine_ext::ResultCode::Error;
                }
                let cursor = &mut *(cursor as *mut <<#struct_name as ::amphora_engine_ext::VTabModule>::Table as ::amphora_engine_ext::VTable>::Cursor);
                let args = ::std::slice::from_raw_parts(argv, argc as usize);
                let idx_str = if idx_str.is_null() {
                    None
                } else {
                    Some((unsafe { ::std::ffi::CStr::from_ptr(idx_str).to_str().unwrap() }, idx_num))
                };
                <<#struct_name as ::amphora_engine_ext::VTabModule>::Table as ::amphora_engine_ext::VTable>::Cursor::filter(cursor, args, idx_str)
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            unsafe extern "C" fn #column_fn_name(
                cursor: *const ::std::ffi::c_void,
                idx: u32,
            ) -> ::amphora_engine_ext::Value {
                if cursor.is_null() {
                    return ::amphora_engine_ext::Value::error(::amphora_engine_ext::ResultCode::Error);
                }
                let cursor = &*(cursor as *const <<#struct_name as ::amphora_engine_ext::VTabModule>::Table as ::amphora_engine_ext::VTable>::Cursor);
                match <<#struct_name as ::amphora_engine_ext::VTabModule>::Table as ::amphora_engine_ext::VTable>::Cursor::column(cursor, idx) {
                    Ok(val) => val,
                    Err(e) => ::amphora_engine_ext::Value::error_with_message(e.to_string())
                }
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            unsafe extern "C" fn #next_fn_name(
                cursor: *const ::std::ffi::c_void,
            ) -> ::amphora_engine_ext::ResultCode {
                if cursor.is_null() {
                    return ::amphora_engine_ext::ResultCode::Error;
                }
                let cursor = &mut *(cursor as *mut <<#struct_name as ::amphora_engine_ext::VTabModule>::Table as ::amphora_engine_ext::VTable>::Cursor);
                <<#struct_name as ::amphora_engine_ext::VTabModule>::Table as ::amphora_engine_ext::VTable>::Cursor::next(cursor)
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            unsafe extern "C" fn #eof_fn_name(
                cursor: *const ::std::ffi::c_void,
            ) -> bool {
                if cursor.is_null() {
                    return true;
                }
                let cursor = &*(cursor as *const <<#struct_name as ::amphora_engine_ext::VTabModule>::Table as ::amphora_engine_ext::VTable>::Cursor);
                <<#struct_name as ::amphora_engine_ext::VTabModule>::Table as ::amphora_engine_ext::VTable>::Cursor::eof(cursor)
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            unsafe extern "C" fn #update_fn_name(
                table: *const ::std::ffi::c_void,
                argc: i32,
                argv: *const ::amphora_engine_ext::Value,
                p_out_rowid: *mut i64,
            ) -> ::amphora_engine_ext::ResultCode {
                if table.is_null() {
                    return ::amphora_engine_ext::ResultCode::Error;
                }

                let table = &mut *(table as *mut <#struct_name as ::amphora_engine_ext::VTabModule>::Table);
                let args = ::std::slice::from_raw_parts(argv, argc as usize);

                let old_rowid = match args.get(0).map(|v| v.value_type()) {
                    Some(::amphora_engine_ext::ValueType::Integer) => args.get(0).unwrap().to_integer(),
                    _ => None,
                };
                let new_rowid = match args.get(1).map(|v| v.value_type()) {
                    Some(::amphora_engine_ext::ValueType::Integer) => args.get(1).unwrap().to_integer(),
                    _ => None,
                };
                let columns = &args[2..];
                match (old_rowid, new_rowid) {
                    // DELETE: old_rowid provided, no new_rowid
                    (Some(old), None) => {
                     if <#struct_name as VTabModule>::Table::delete(table, old).is_err() {
                            return ::amphora_engine_ext::ResultCode::Error;
                      }
                            return ::amphora_engine_ext::ResultCode::OK;
                    }
                    // UPDATE: old_rowid provided and new_rowid may exist
                    (Some(old), Some(new)) => {
                        if <#struct_name as VTabModule>::Table::update(table, old, &columns).is_err() {
                            return ::amphora_engine_ext::ResultCode::Error;
                        }
                        return ::amphora_engine_ext::ResultCode::OK;
                    }
                    // INSERT: no old_rowid (old_rowid = None)
                    (None, _) => {
                        if let Ok(rowid) = <#struct_name as VTabModule>::Table::insert(table, &columns) {
                            if !p_out_rowid.is_null() {
                                *p_out_rowid = rowid;
                                 return ::amphora_engine_ext::ResultCode::RowID;
                            }
                            return ::amphora_engine_ext::ResultCode::OK;
                        }
                    }
                }
                return ::amphora_engine_ext::ResultCode::Error;
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            pub unsafe extern "C" fn #rowid_fn_name(cursor: *const ::std::ffi::c_void) -> i64 {
                if cursor.is_null() {
                    return -1;
                }
                let cursor = &*(cursor as *const <<#struct_name as ::amphora_engine_ext::VTabModule>::Table as ::amphora_engine_ext::VTable>::Cursor);
                <<#struct_name as ::amphora_engine_ext::VTabModule>::Table as ::amphora_engine_ext::VTable>::Cursor::rowid(cursor)
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            unsafe extern "C" fn #destroy_fn_name(
                table: *const ::std::ffi::c_void,
            ) -> ::amphora_engine_ext::ResultCode {
                if table.is_null() {
                    return ::amphora_engine_ext::ResultCode::Error;
                }

                // Take ownership of the table so it can be properly dropped.
                let mut table: ::std::boxed::Box<<#struct_name as ::amphora_engine_ext::VTabModule>::Table> =
                ::std::boxed::Box::from_raw(table as *mut <#struct_name as ::amphora_engine_ext::VTabModule>::Table);
                if <#struct_name as VTabModule>::Table::destroy(&mut *table).is_err() {
                    return ::amphora_engine_ext::ResultCode::Error;
                }

                return ::amphora_engine_ext::ResultCode::OK;
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            pub unsafe extern "C" fn #best_idx_fn_name(
                constraints: *const ::amphora_engine_ext::ConstraintInfo,
                n_constraints: i32,
                order_by: *const ::amphora_engine_ext::OrderByInfo,
                n_order_by: i32,
            ) -> ::amphora_engine_ext::ExtIndexInfo {
                let constraints = if n_constraints > 0 { std::slice::from_raw_parts(constraints, n_constraints as usize) } else { &[] };
                let order_by = if n_order_by > 0 { std::slice::from_raw_parts(order_by, n_order_by as usize) } else { &[] };
                match <#struct_name as ::amphora_engine_ext::VTabModule>::Table::best_index(constraints, order_by) {
                    Ok(index_info) => index_info.to_ffi(),
                    Err(e) => ::amphora_engine_ext::ExtIndexInfo::error(e),
                }
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            pub unsafe extern "C" fn #begin_fn_name(
                table: *mut ::std::ffi::c_void,
            ) -> ::amphora_engine_ext::ResultCode {
                let table = if table.is_null() {
                    return ::amphora_engine_ext::ResultCode::Error;
                } else {
                    &mut *(table as *mut <#struct_name as ::amphora_engine_ext::VTabModule>::Table)
                };
                if <#struct_name as ::amphora_engine_ext::VTabModule>::Table::begin(table).is_err() {
                    return ::amphora_engine_ext::ResultCode::Error;
                }
                ::amphora_engine_ext::ResultCode::OK
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            pub unsafe extern "C" fn #rollback_fn_name(
                table: *mut ::std::ffi::c_void,
            ) -> ::amphora_engine_ext::ResultCode {
                let table = if table.is_null() {
                    return ::amphora_engine_ext::ResultCode::Error;
                } else {
                    &mut *(table as *mut <#struct_name as ::amphora_engine_ext::VTabModule>::Table)
                };
                if <#struct_name as ::amphora_engine_ext::VTabModule>::Table::rollback(table).is_err() {
                    return ::amphora_engine_ext::ResultCode::Error;
                }
                ::amphora_engine_ext::ResultCode::OK
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            pub unsafe extern "C" fn #commit_fn_name(
                table: *mut ::std::ffi::c_void,
            ) -> ::amphora_engine_ext::ResultCode {
                let table = if table.is_null() {
                    return ::amphora_engine_ext::ResultCode::Error;
                } else {
                    &mut *(table as *mut <#struct_name as ::amphora_engine_ext::VTabModule>::Table)
                };
                if <#struct_name as ::amphora_engine_ext::VTabModule>::Table::commit(table).is_err() {
                    return ::amphora_engine_ext::ResultCode::Error;
                }
                ::amphora_engine_ext::ResultCode::OK
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            pub unsafe extern "C" fn #rename_fn_name(
                table: *mut ::std::ffi::c_void,
                name: *const ::std::ffi::c_char,
            ) -> ::amphora_engine_ext::ResultCode {
                let table = if table.is_null() {
                    return ::amphora_engine_ext::ResultCode::Error;
                } else {
                    &mut *(table as *mut <#struct_name as ::amphora_engine_ext::VTabModule>::Table)
                };
                let name_str = if name.is_null() {
                    return ::amphora_engine_ext::ResultCode::Error;
                } else {
                    match ::std::ffi::CStr::from_ptr(name).to_str() {
                        Ok(s) => s,
                        Err(_) => return ::amphora_engine_ext::ResultCode::Error,
                    }
                };
                if <#struct_name as ::amphora_engine_ext::VTabModule>::Table::rename(table, name_str).is_err() {
                    return ::amphora_engine_ext::ResultCode::Error;
                }
                ::amphora_engine_ext::ResultCode::OK
            }

            #[cfg_attr(not(amphora_engine_core), no_mangle)]
            #[allow(non_snake_case)]
            pub unsafe extern "C" fn #register_fn_name(
                api: *const ::amphora_engine_ext::ExtensionApi
            ) -> ::amphora_engine_ext::ResultCode {
                if api.is_null() {
                    return ::amphora_engine_ext::ResultCode::Error;
                }
                let api = &*api;
                let name = <#struct_name as ::amphora_engine_ext::VTabModule>::NAME;
                let name_c = ::std::ffi::CString::new(name).unwrap().into_raw() as *const ::std::ffi::c_char;
                let module = ::amphora_engine_ext::VTabModuleImpl {
                    name: name_c,
                    readonly: <#struct_name as ::amphora_engine_ext::VTabModule>::READONLY,
                    create: Self::#create_fn_name,
                    open: Self::#open_fn_name,
                    close: Self::#close_fn_name,
                    filter: Self::#filter_fn_name,
                    column: Self::#column_fn_name,
                    next: Self::#next_fn_name,
                    eof: Self::#eof_fn_name,
                    update: Self::#update_fn_name,
                    rowid: Self::#rowid_fn_name,
                    destroy: Self::#destroy_fn_name,
                    best_idx: Self::#best_idx_fn_name,
                    begin: Self::#begin_fn_name,
                    rollback: Self::#rollback_fn_name,
                    commit: Self::#commit_fn_name,
                    rename: Self::#rename_fn_name,
                };
                (api.register_vtab_module)(api.ctx, name_c, module, <#struct_name as ::amphora_engine_ext::VTabModule>::VTAB_KIND)
            }
        }
    };

    TokenStream::from(expanded)
}
