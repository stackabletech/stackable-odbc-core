/// Generates all ODBC FFI forwarding stubs for a given backend.
///
/// Place this in a driver's `lib.rs`:
/// ```ignore
/// stackable_odbc_core::forward_ffi!(crate::backend::MyBackend);
/// ```
#[macro_export]
macro_rules! forward_ffi {
    ($B:ty) => {
        // ---------------------------------------------------------------------------
        // Handle management
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLAllocHandle(
            handle_type: i16,
            input_handle: *mut ::std::ffi::c_void,
            output_handle: *mut *mut ::std::ffi::c_void,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::handle::sql_alloc_handle::<$B>(
                    handle_type,
                    input_handle,
                    output_handle,
                )
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLFreeHandle(
            handle_type: i16,
            handle: *mut ::std::ffi::c_void,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::handle::sql_free_handle::<$B>(handle_type, handle) }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLFreeStmt(
            stmt: *mut ::std::ffi::c_void,
            option: u16,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::handle::sql_free_stmt::<$B>(stmt, option) }
        }

        // ---------------------------------------------------------------------------
        // Environment
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLSetEnvAttr(
            env: *mut ::std::ffi::c_void,
            attr: i32,
            value: *mut ::std::ffi::c_void,
            len: i32,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::env::sql_set_env_attr::<$B>(env, attr, value, len) }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetEnvAttr(
            env: *mut ::std::ffi::c_void,
            attr: i32,
            value: *mut ::std::ffi::c_void,
            buf_len: i32,
            str_len: *mut i32,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::env::sql_get_env_attr::<$B>(env, attr, value, buf_len, str_len) }
        }

        // ---------------------------------------------------------------------------
        // Connection
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLDriverConnectW(
            conn: *mut ::std::ffi::c_void,
            window: *mut ::std::ffi::c_void,
            in_str: *const u16,
            in_len: i16,
            out_str: *mut u16,
            buf_len: i16,
            out_len: *mut i16,
            completion: u16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::connect::sql_driver_connect_w::<$B>(
                    conn, window, in_str, in_len, out_str, buf_len, out_len, completion,
                )
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLDisconnect(
            conn: *mut ::std::ffi::c_void,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::connect::sql_disconnect::<$B>(conn) }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLConnectW(
            conn: *mut ::std::ffi::c_void,
            server: *const u16,
            s_len: i16,
            user: *const u16,
            u_len: i16,
            auth: *const u16,
            a_len: i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::connect::sql_connect_w::<$B>(
                    conn, server, s_len, user, u_len, auth, a_len,
                )
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLNativeSqlW(
            conn: *mut ::std::ffi::c_void,
            in_sql: *const u16,
            in_len: i32,
            out_sql: *mut u16,
            buf_len: i32,
            out_len: *mut i32,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::connect::sql_native_sql_w::<$B>(
                    conn, in_sql, in_len, out_sql, buf_len, out_len,
                )
            }
        }

        // ---------------------------------------------------------------------------
        // Diagnostics
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetDiagRecW(
            handle_type: i16,
            handle: *mut ::std::ffi::c_void,
            rec_num: i16,
            state: *mut u16,
            native_err: *mut i32,
            msg: *mut u16,
            buf_len: i16,
            text_len: *mut i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::diag::sql_get_diag_rec_w::<$B>(
                    handle_type,
                    handle,
                    rec_num,
                    state,
                    native_err,
                    msg,
                    buf_len,
                    text_len,
                )
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetDiagFieldW(
            handle_type: i16,
            handle: *mut ::std::ffi::c_void,
            rec_num: i16,
            diag_id: i16,
            diag_info: *mut ::std::ffi::c_void,
            buf_len: i16,
            str_len: *mut i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::diag::sql_get_diag_field_w::<$B>(
                    handle_type,
                    handle,
                    rec_num,
                    diag_id,
                    diag_info,
                    buf_len,
                    str_len,
                )
            }
        }

        // ---------------------------------------------------------------------------
        // Query execution
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLExecDirectW(
            stmt: *mut ::std::ffi::c_void,
            text: *const u16,
            len: i32,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::execute::sql_exec_direct_w::<$B>(stmt, text, len) }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLPrepareW(
            stmt: *mut ::std::ffi::c_void,
            text: *const u16,
            len: i32,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::execute::sql_prepare_w::<$B>(stmt, text, len) }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLExecute(
            stmt: *mut ::std::ffi::c_void,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::execute::sql_execute::<$B>(stmt) }
        }

        // ---------------------------------------------------------------------------
        // Fetch
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLFetch(
            stmt: *mut ::std::ffi::c_void,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::fetch::sql_fetch::<$B>(stmt) }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetData(
            stmt: *mut ::std::ffi::c_void,
            col: u16,
            target_type: i16,
            value: *mut ::std::ffi::c_void,
            len: isize,
            ind: *mut isize,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::fetch::sql_get_data::<$B>(stmt, col, target_type, value, len, ind)
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLFetchScroll(
            stmt: *mut ::std::ffi::c_void,
            orientation: i16,
            offset: isize,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::fetch::sql_fetch_scroll::<$B>(stmt, orientation, offset) }
        }

        // ---------------------------------------------------------------------------
        // Cursor
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLNumResultCols(
            stmt: *mut ::std::ffi::c_void,
            count: *mut i16,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::cursor::sql_num_result_cols::<$B>(stmt, count) }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLRowCount(
            stmt: *mut ::std::ffi::c_void,
            count: *mut isize,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::cursor::sql_row_count::<$B>(stmt, count) }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLMoreResults(
            stmt: *mut ::std::ffi::c_void,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::cursor::sql_more_results::<$B>(stmt) }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLCloseCursor(
            stmt: *mut ::std::ffi::c_void,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::cursor::sql_close_cursor::<$B>(stmt) }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLCancel(
            stmt: *mut ::std::ffi::c_void,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::cursor::sql_cancel::<$B>(stmt) }
        }

        // ---------------------------------------------------------------------------
        // Metadata
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLDescribeColW(
            stmt: *mut ::std::ffi::c_void,
            col: u16,
            name: *mut u16,
            buf_len: i16,
            name_len: *mut i16,
            data_type: *mut i16,
            size: *mut $crate::types::ULen,
            decimal: *mut i16,
            nullable: *mut i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::metadata::sql_describe_col_w::<$B>(
                    stmt, col, name, buf_len, name_len, data_type, size, decimal, nullable,
                )
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLColAttributeW(
            stmt: *mut ::std::ffi::c_void,
            col: u16,
            field: u16,
            char_attr: *mut ::std::ffi::c_void,
            buf_len: i16,
            str_len: *mut i16,
            num_attr: *mut isize,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::metadata::sql_col_attribute_w::<$B>(
                    stmt, col, field, char_attr, buf_len, str_len, num_attr,
                )
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLTablesW(
            stmt: *mut ::std::ffi::c_void,
            cat: *const u16,
            cat_len: i16,
            schema: *const u16,
            schema_len: i16,
            table: *const u16,
            table_len: i16,
            table_type: *const u16,
            type_len: i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::metadata::sql_tables_w::<$B>(
                    stmt, cat, cat_len, schema, schema_len, table, table_len, table_type, type_len,
                )
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLColumnsW(
            stmt: *mut ::std::ffi::c_void,
            cat: *const u16,
            cat_len: i16,
            schema: *const u16,
            schema_len: i16,
            table: *const u16,
            table_len: i16,
            col: *const u16,
            col_len: i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::metadata::sql_columns_w::<$B>(
                    stmt, cat, cat_len, schema, schema_len, table, table_len, col, col_len,
                )
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLStatisticsW(
            stmt: *mut ::std::ffi::c_void,
            cat: *const u16,
            cat_len: i16,
            schema: *const u16,
            schema_len: i16,
            table: *const u16,
            table_len: i16,
            unique: u16,
            reserved: u16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::metadata::sql_statistics_w::<$B>(
                    stmt, cat, cat_len, schema, schema_len, table, table_len, unique, reserved,
                )
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLSpecialColumnsW(
            stmt: *mut ::std::ffi::c_void,
            id_type: u16,
            cat: *const u16,
            cat_len: i16,
            schema: *const u16,
            schema_len: i16,
            table: *const u16,
            table_len: i16,
            scope: u16,
            nullable: u16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::metadata::sql_special_columns_w::<$B>(
                    stmt, id_type, cat, cat_len, schema, schema_len, table, table_len, scope,
                    nullable,
                )
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLPrimaryKeysW(
            stmt: *mut ::std::ffi::c_void,
            cat: *const u16,
            cat_len: i16,
            schema: *const u16,
            schema_len: i16,
            table: *const u16,
            table_len: i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::metadata::sql_primary_keys_w::<$B>(
                    stmt, cat, cat_len, schema, schema_len, table, table_len,
                )
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLForeignKeysW(
            stmt: *mut ::std::ffi::c_void,
            pk_cat: *const u16,
            pk_cat_len: i16,
            pk_schema: *const u16,
            pk_schema_len: i16,
            pk_table: *const u16,
            pk_table_len: i16,
            fk_cat: *const u16,
            fk_cat_len: i16,
            fk_schema: *const u16,
            fk_schema_len: i16,
            fk_table: *const u16,
            fk_table_len: i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::metadata::sql_foreign_keys_w::<$B>(
                    stmt,
                    pk_cat,
                    pk_cat_len,
                    pk_schema,
                    pk_schema_len,
                    pk_table,
                    pk_table_len,
                    fk_cat,
                    fk_cat_len,
                    fk_schema,
                    fk_schema_len,
                    fk_table,
                    fk_table_len,
                )
            }
        }

        // ---------------------------------------------------------------------------
        // Info
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetInfoW(
            conn: *mut ::std::ffi::c_void,
            info_type: u16,
            value: *mut ::std::ffi::c_void,
            buf_len: i16,
            str_len: *mut i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::info::sql_get_info_w::<$B>(conn, info_type, value, buf_len, str_len)
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetFunctions(
            conn: *mut ::std::ffi::c_void,
            func_id: u16,
            supported: *mut u16,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::info::sql_get_functions::<$B>(conn, func_id, supported) }
        }

        /// Non-W alias: SQLGetTypeInfo has no string parameters; some Driver Managers
        /// look up this name in addition to SQLGetTypeInfoW.
        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetTypeInfo(
            stmt: *mut ::std::ffi::c_void,
            data_type: i16,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::info::sql_get_type_info::<$B>(stmt, data_type) }
        }

        /// W-suffix alias — Windows DM may look up either name.
        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetTypeInfoW(
            stmt: *mut ::std::ffi::c_void,
            data_type: i16,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::info::sql_get_type_info::<$B>(stmt, data_type) }
        }

        // ---------------------------------------------------------------------------
        // Connection / Statement attributes
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLSetConnectAttrW(
            conn: *mut ::std::ffi::c_void,
            attr: i32,
            value: *mut ::std::ffi::c_void,
            len: i32,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::connect_attr::sql_set_connect_attr_w::<$B>(conn, attr, value, len)
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetConnectAttrW(
            conn: *mut ::std::ffi::c_void,
            attr: i32,
            value: *mut ::std::ffi::c_void,
            buf_len: i32,
            str_len: *mut i32,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::connect_attr::sql_get_connect_attr_w::<$B>(
                    conn, attr, value, buf_len, str_len,
                )
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLSetStmtAttrW(
            stmt: *mut ::std::ffi::c_void,
            attr: i32,
            value: *mut ::std::ffi::c_void,
            len: i32,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::stmt_attr::sql_set_stmt_attr_w::<$B>(stmt, attr, value, len) }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetStmtAttrW(
            stmt: *mut ::std::ffi::c_void,
            attr: i32,
            value: *mut ::std::ffi::c_void,
            buf_len: i32,
            str_len: *mut i32,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::stmt_attr::sql_get_stmt_attr_w::<$B>(
                    stmt, attr, value, buf_len, str_len,
                )
            }
        }

        // ---------------------------------------------------------------------------
        // Parameters / Binding
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLBindCol(
            stmt: *mut ::std::ffi::c_void,
            col: u16,
            target_type: i16,
            value: *mut ::std::ffi::c_void,
            len: isize,
            ind: *mut isize,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::bind::sql_bind_col::<$B>(stmt, col, target_type, value, len, ind)
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLBindParameter(
            stmt: *mut ::std::ffi::c_void,
            param: u16,
            io_type: i16,
            c_type: i16,
            sql_type: i16,
            col_size: $crate::types::ULen,
            dec: i16,
            value: *mut ::std::ffi::c_void,
            len: isize,
            ind: *mut isize,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::params::sql_bind_parameter::<$B>(
                    stmt, param, io_type, c_type, sql_type, col_size, dec, value, len, ind,
                )
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLNumParams(
            stmt: *mut ::std::ffi::c_void,
            count: *mut i16,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::params::sql_num_params::<$B>(stmt, count) }
        }

        // ---------------------------------------------------------------------------
        // Transactions
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLEndTran(
            handle_type: i16,
            handle: *mut ::std::ffi::c_void,
            completion_type: i16,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::tran::sql_end_tran::<$B>(handle_type, handle, completion_type) }
        }

        // ---------------------------------------------------------------------------
        // Stubs — less-common catalog functions
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLProceduresW(
            stmt: *mut ::std::ffi::c_void,
            cat: *const u16,
            cat_len: i16,
            schema: *const u16,
            schema_len: i16,
            proc_name: *const u16,
            proc_len: i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::metadata::sql_procedures_w::<$B>(
                    stmt, cat, cat_len, schema, schema_len, proc_name, proc_len,
                )
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLProcedureColumnsW(
            stmt: *mut ::std::ffi::c_void,
            cat: *const u16,
            cat_len: i16,
            schema: *const u16,
            schema_len: i16,
            proc_name: *const u16,
            proc_len: i16,
            col: *const u16,
            col_len: i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::metadata::sql_procedure_columns_w::<$B>(
                    stmt, cat, cat_len, schema, schema_len, proc_name, proc_len, col, col_len,
                )
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetCursorNameW(
            stmt: *mut ::std::ffi::c_void,
            name: *mut u16,
            buf_len: i16,
            name_len: *mut i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::cursor::sql_get_cursor_name_w::<$B>(stmt, name, buf_len, name_len)
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLSetCursorNameW(
            stmt: *mut ::std::ffi::c_void,
            name: *const u16,
            name_len: i16,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::cursor::sql_set_cursor_name_w::<$B>(stmt, name, name_len) }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLColumnPrivilegesW(
            stmt: *mut ::std::ffi::c_void,
            cat: *const u16,
            cat_len: i16,
            schema: *const u16,
            schema_len: i16,
            table: *const u16,
            table_len: i16,
            col: *const u16,
            col_len: i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::metadata::sql_column_privileges_w::<$B>(
                    stmt, cat, cat_len, schema, schema_len, table, table_len, col, col_len,
                )
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLTablePrivilegesW(
            stmt: *mut ::std::ffi::c_void,
            cat: *const u16,
            cat_len: i16,
            schema: *const u16,
            schema_len: i16,
            table: *const u16,
            table_len: i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::metadata::sql_table_privileges_w::<$B>(
                    stmt, cat, cat_len, schema, schema_len, table, table_len,
                )
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLParamData(
            stmt: *mut ::std::ffi::c_void,
            value: *mut *mut ::std::ffi::c_void,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::params::sql_param_data::<$B>(stmt, value) }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLPutData(
            stmt: *mut ::std::ffi::c_void,
            data: *mut ::std::ffi::c_void,
            len: isize,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::params::sql_put_data::<$B>(stmt, data, len) }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLDescribeParam(
            stmt: *mut ::std::ffi::c_void,
            param: u16,
            data_type: *mut i16,
            size: *mut $crate::types::ULen,
            dec: *mut i16,
            nullable: *mut i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::params::sql_describe_param::<$B>(
                    stmt, param, data_type, size, dec, nullable,
                )
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLBrowseConnectW(
            conn: *mut ::std::ffi::c_void,
            in_str: *const u16,
            in_len: i16,
            out_str: *mut u16,
            out_buf: i16,
            out_len: *mut i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::connect::sql_browse_connect_w::<$B>(
                    conn, in_str, in_len, out_str, out_buf, out_len,
                )
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLBulkOperations(
            stmt: *mut ::std::ffi::c_void,
            operation: i16,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::cursor::sql_bulk_operations::<$B>(stmt, operation) }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLSetPos(
            stmt: *mut ::std::ffi::c_void,
            row: u64,
            operation: u16,
            lock_type: u16,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::cursor::sql_set_pos::<$B>(stmt, row, operation, lock_type) }
        }

        // ---------------------------------------------------------------------------
        // Windows DM compatibility — functions that require no backend
        // ---------------------------------------------------------------------------

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLExtendedFetch(
            stmt: *mut ::std::ffi::c_void,
            fetch_type: u16,
            row_offset: isize,
            rows_fetched: *mut usize,
            row_status: *mut u16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::fetch::sql_extended_fetch::<$B>(
                    stmt,
                    fetch_type,
                    row_offset,
                    rows_fetched,
                    row_status,
                )
            }
        }

        // SQLSetScrollOptions is deliberately not exported. Its spec page defines
        // no diagnostics table at all, and documents what the Driver Manager does
        // "for an application working with an ODBC 3.x driver that does not
        // support SQLSetScrollOptions": it sets SQL_ROWSET_SIZE itself. unixODBC's
        // DM implements that mapping in full -- SQLGetInfo to validate the
        // requested concurrency, then SQLSetStmtAttr for SQL_ATTR_CONCURRENCY,
        // SQL_ATTR_CURSOR_TYPE, SQL_ATTR_KEYSET_SIZE and SQL_ROWSET_SIZE -- and
        // dispatches to the driver's own entry point only when the driver exports
        // one. So exporting anything here replaces a capability-checked mapping,
        // derived from this driver's own SQLGetInfo answers, with whatever the
        // driver says instead. psqlODBC reaches the same conclusion: it never
        // defines the symbol.
        //
        // See CORE_UNEXPORTED_FUNCTIONS in function_id.rs, which is what stops
        // SQLGetFunctions claiming it.

        #[allow(non_snake_case, clippy::missing_safety_doc, clippy::too_many_arguments)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetDescRecW(
            desc: *mut ::std::ffi::c_void,
            rec: i16,
            name: *mut u16,
            buf_len: i16,
            str_len: *mut i16,
            type_ptr: *mut i16,
            sub_type: *mut i16,
            length: *mut isize,
            precision: *mut i16,
            scale: *mut i16,
            nullable: *mut i16,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::desc::sql_get_desc_rec_w::<$B>(
                    desc, rec, name, buf_len, str_len, type_ptr, sub_type, length, precision,
                    scale, nullable,
                )
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLGetDescFieldW(
            desc: *mut ::std::ffi::c_void,
            rec: i16,
            field: i16,
            value: *mut ::std::ffi::c_void,
            buf_len: i32,
            str_len: *mut i32,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::desc::sql_get_desc_field_w::<$B>(
                    desc, rec, field, value, buf_len, str_len,
                )
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLSetDescFieldW(
            desc: *mut ::std::ffi::c_void,
            rec: i16,
            field: i16,
            value: *mut ::std::ffi::c_void,
            buf_len: i32,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::desc::sql_set_desc_field_w::<$B>(desc, rec, field, value, buf_len)
            }
        }

        #[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLSetDescRec(
            desc: *mut ::std::ffi::c_void,
            rec: i16,
            value_type: i16,
            subtype: i16,
            length: isize,
            precision: i16,
            scale: i16,
            data: *mut ::std::ffi::c_void,
            str_len: *mut isize,
            indicator: *mut isize,
        ) -> $crate::types::SqlReturn {
            unsafe {
                $crate::ffi::desc::sql_set_desc_rec::<$B>(
                    desc, rec, value_type, subtype, length, precision, scale, data, str_len,
                    indicator,
                )
            }
        }

        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SQLCopyDesc(
            source: *mut ::std::ffi::c_void,
            target: *mut ::std::ffi::c_void,
        ) -> $crate::types::SqlReturn {
            unsafe { $crate::ffi::desc::sql_copy_desc::<$B>(source, target) }
        }

        // ---------------------------------------------------------------------------
        // Setup (Windows only)
        // ---------------------------------------------------------------------------

        #[cfg(windows)]
        #[allow(non_snake_case, clippy::missing_safety_doc)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn ConfigDSNW(
            hwnd_parent: *mut ::std::ffi::c_void,
            f_request: u16,
            lpsz_driver: *const u16,
            lpsz_attributes: *const u16,
        ) -> i32 {
            unsafe {
                $crate::ffi::setup::config_dsn_w(
                    hwnd_parent,
                    f_request,
                    lpsz_driver,
                    lpsz_attributes,
                )
            }
        }
    }; // end of macro arm
}

/// Expanding the macro is the assertion.
///
/// A `macro_rules!` body is stored as token trees and is never name-resolved
/// until something invokes it. No driver crate is built here, so without this
/// module the whole macro arm above is uncompiled text: a wrong parameter type
/// in an exported signature, a swapped or missing argument in a forwarding
/// call, or a typo in a `$crate::ffi::` path compiles, tests, clippies and
/// pre-commits clean in this repository, and breaks every downstream driver on
/// its next dependency bump.
///
/// The generated `#[unsafe(no_mangle)]` symbols land in the unit-test binary,
/// which also links `libodbc` for its own `SQL*` symbols. That is not a
/// conflict: `libodbc` is a dynamic library, so these definitions take
/// precedence, and no test in this crate routes a call through the Driver
/// Manager.
#[cfg(test)]
mod expansion {
    crate::forward_ffi!(crate::test_utils::MockBackend);

    /// Pins [`CORE_EXPORTED_FUNCTIONS`] to symbols that actually exist.
    ///
    /// Each entry takes the address of the generated entry point, so a
    /// `FunctionId` listed as exported without a matching `forward_ffi!` arm is
    /// a compile error rather than a null pointer the Windows Driver Manager
    /// discovers at dispatch time.
    #[test]
    fn every_exported_function_id_has_a_generated_symbol() {
        use crate::function_id::{CORE_EXPORTED_FUNCTIONS, FunctionId};

        let pairs: &[(FunctionId, *const ())] = &[
            (FunctionId::BindCol, SQLBindCol as *const ()),
            (FunctionId::Cancel, SQLCancel as *const ()),
            (FunctionId::ColAttribute, SQLColAttributeW as *const ()),
            (FunctionId::Connect, SQLConnectW as *const ()),
            (FunctionId::DescribeCol, SQLDescribeColW as *const ()),
            (FunctionId::Disconnect, SQLDisconnect as *const ()),
            (FunctionId::ExecDirect, SQLExecDirectW as *const ()),
            (FunctionId::Execute, SQLExecute as *const ()),
            (FunctionId::Fetch, SQLFetch as *const ()),
            (FunctionId::FreeStmt, SQLFreeStmt as *const ()),
            (FunctionId::GetCursorName, SQLGetCursorNameW as *const ()),
            (FunctionId::NumResultCols, SQLNumResultCols as *const ()),
            (FunctionId::Prepare, SQLPrepareW as *const ()),
            (FunctionId::RowCount, SQLRowCount as *const ()),
            (FunctionId::SetCursorName, SQLSetCursorNameW as *const ()),
            (FunctionId::BulkOperations, SQLBulkOperations as *const ()),
            (FunctionId::Columns, SQLColumnsW as *const ()),
            (FunctionId::DriverConnect, SQLDriverConnectW as *const ()),
            (FunctionId::GetData, SQLGetData as *const ()),
            (FunctionId::GetFunctions, SQLGetFunctions as *const ()),
            (FunctionId::GetInfo, SQLGetInfoW as *const ()),
            (FunctionId::GetTypeInfo, SQLGetTypeInfo as *const ()),
            (FunctionId::ParamData, SQLParamData as *const ()),
            (FunctionId::PutData, SQLPutData as *const ()),
            (FunctionId::SpecialColumns, SQLSpecialColumnsW as *const ()),
            (FunctionId::Statistics, SQLStatisticsW as *const ()),
            (FunctionId::Tables, SQLTablesW as *const ()),
            (FunctionId::BrowseConnect, SQLBrowseConnectW as *const ()),
            (
                FunctionId::ColumnPrivileges,
                SQLColumnPrivilegesW as *const (),
            ),
            (FunctionId::DescribeParam, SQLDescribeParam as *const ()),
            (FunctionId::ExtendedFetch, SQLExtendedFetch as *const ()),
            (FunctionId::ForeignKeys, SQLForeignKeysW as *const ()),
            (FunctionId::MoreResults, SQLMoreResults as *const ()),
            (FunctionId::NativeSql, SQLNativeSqlW as *const ()),
            (FunctionId::NumParams, SQLNumParams as *const ()),
            (FunctionId::PrimaryKeys, SQLPrimaryKeysW as *const ()),
            (
                FunctionId::ProcedureColumns,
                SQLProcedureColumnsW as *const (),
            ),
            (FunctionId::Procedures, SQLProceduresW as *const ()),
            (FunctionId::SetPos, SQLSetPos as *const ()),
            (
                FunctionId::TablePrivileges,
                SQLTablePrivilegesW as *const (),
            ),
            (FunctionId::BindParameter, SQLBindParameter as *const ()),
            (FunctionId::AllocHandle, SQLAllocHandle as *const ()),
            (FunctionId::CloseCursor, SQLCloseCursor as *const ()),
            (FunctionId::EndTran, SQLEndTran as *const ()),
            (FunctionId::FreeHandle, SQLFreeHandle as *const ()),
            (FunctionId::GetConnectAttr, SQLGetConnectAttrW as *const ()),
            (FunctionId::GetDescField, SQLGetDescFieldW as *const ()),
            (FunctionId::GetDescRec, SQLGetDescRecW as *const ()),
            (FunctionId::GetDiagField, SQLGetDiagFieldW as *const ()),
            (FunctionId::GetDiagRec, SQLGetDiagRecW as *const ()),
            (FunctionId::GetEnvAttr, SQLGetEnvAttr as *const ()),
            (FunctionId::GetStmtAttr, SQLGetStmtAttrW as *const ()),
            (FunctionId::SetConnectAttr, SQLSetConnectAttrW as *const ()),
            (FunctionId::SetDescField, SQLSetDescFieldW as *const ()),
            (FunctionId::SetDescRec, SQLSetDescRec as *const ()),
            (FunctionId::CopyDesc, SQLCopyDesc as *const ()),
            (FunctionId::SetEnvAttr, SQLSetEnvAttr as *const ()),
            (FunctionId::SetStmtAttr, SQLSetStmtAttrW as *const ()),
            (FunctionId::FetchScroll, SQLFetchScroll as *const ()),
        ];

        assert_eq!(
            pairs.len(),
            CORE_EXPORTED_FUNCTIONS.len(),
            "every entry in CORE_EXPORTED_FUNCTIONS needs a symbol here"
        );
        for (id, ptr) in pairs {
            assert!(
                CORE_EXPORTED_FUNCTIONS.contains(id),
                "{id:?} has a symbol but is not listed as exported"
            );
            assert!(!ptr.is_null());
        }
    }
}
