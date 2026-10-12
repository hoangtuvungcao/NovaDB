//! Fast-path screen for the dialect transpiler.
//!
//! `normalize_single_batch` is a long sequence of regex passes. Each pass only
//! rewrites text it matches, so when *none* of the passes' patterns matches a
//! statement, every pass is the identity and the statement can skip all of
//! them. Running ~350 individual regexes costs ~55 us even on a trivial
//! `SELECT`; one pre-built [`RegexSet`] answers "does any pass match?" in a
//! fraction of a microsecond.
//!
//! `PASS_PATTERNS` must contain every pattern passed to `cached_regex!` inside
//! `normalize_single_batch`. The `pass_patterns_match_source` test parses
//! `lib.rs` and fails when the list drifts from the source.

use std::sync::{LazyLock, Once};

use regex::{RegexSet, RegexSetBuilder};

pub(crate) static PASS_PATTERNS: &[&str] = &[
    r"(?i)\b(geometry|geography)::Point\s*\(([^,()]+),([^,()]+),([^()]+)\)(?:\s*\.\s*(?:ToString|STAsText)\s*\(\s*\))?",
    r"(?i)\bSET\s+(?:NOCOUNT|ANSI_NULLS|QUOTED_IDENTIFIER|XACT_ABORT|ARITHABORT|ANSI_WARNINGS|ANSI_PADDING|NUMERIC_ROUNDABORT|CONCAT_NULL_YIELDS_NULL|TEXTSIZE|ROWCOUNT|STATISTICS\s+(?:TIME|IO|PROFILE)|SHOWPLAN_(?:ALL|TEXT|XML)|DEADLOCK_PRIORITY|LOCK_TIMEOUT|IMPLICIT_TRANSACTIONS|DATEFIRST|DATEFORMAT|TRANSACTION\s+ISOLATION\s+LEVEL|IDENTITY_INSERT)\s+[^;]+;?",
    r"(?i)\bBEGIN\s+(?:TRANSACTION|TRAN)\b",
    r"(?is)\b(?:BEGIN\s+(?:TRANSACTION|TRAN|WORK)(?:\s+[a-zA-Z0-9_#$]+)?|COMMIT\s+(?:TRANSACTION|TRAN|WORK)(?:\s+[a-zA-Z0-9_#$]+)?|ROLLBACK\s+(?:TRANSACTION|TRAN|WORK)(?:\s+[a-zA-Z0-9_#$]+)?|SAVE\s+(?:TRANSACTION|TRAN)(?:\s+[a-zA-Z0-9_#$]+)?)\s*;?",
    r"(?i)\b(?:COMMIT|ROLLBACK)\b\s*;?",
    r"(?is)\bIF\s+DB_ID\s*\([^)]*\)\s+IS\s+(?:NOT\s+)?NULL\s+BEGIN[\s\S]*?END;?",
    r"(?i)\bCREATE\s+DATABASE\s+([a-zA-Z0-9_#$]+);?",
    r"(?is)\bALTER\s+DATABASE\s+SCOPED\s+CONFIGURATION\s+[\s\S]*?;",
    r"(?is)\bALTER\s+DATABASE\s+[a-zA-Z0-9_#$]+\s+[\s\S]*?;",
    r"(?i)\bUSE\s+([a-zA-Z0-9_#$]+);?",
    r"(?is)\bDBCC\s+[a-zA-Z0-9_#$]+(?:\s*\([^)]*\))?(?:\s+WITH\s+[^;]+)?;?",
    r"(?is)\b(?:CREATE\s+(?:MASTER\s+KEY|CERTIFICATE|SYMMETRIC\s+KEY)|(?:OPEN|CLOSE)\s+SYMMETRIC\s+KEY)\b[\s\S]*?;",
    r"(?is)\b(?:CREATE\s+(?:MESSAGE\s+TYPE|CONTRACT|QUEUE|SERVICE)|BEGIN\s+DIALOG(?:\s+CONVERSATION)?|SEND\s+ON\s+CONVERSATION|END\s+CONVERSATION)\b[\s\S]*?;",
    r"(?is)\bCREATE\s+(?:DEFAULT|RULE)\s+[a-zA-Z0-9_#$.]+\s+AS\s+[\s\S]*?;",
    r"(?is)\bCREATE\s+XML\s+SCHEMA\s+COLLECTION\s+[a-zA-Z0-9_#$.]+\s+AS\s+[\s\S]*?;",
    r"(?is)\b(?:CREATE\s+(?:USER|ROLE)|ALTER\s+ROLE|GRANT|DENY|REVOKE|EXECUTE\s+AS|REVERT\b)[\s\S]*?;",
    r"(?is)\bWAITFOR\s+[\s\S]*?;",
    r"(?i)\bGOTO\s+[a-zA-Z0-9_#$]+;?",
    r"(?im)^[ \t]*[a-zA-Z0-9_#$]+:[ \t]*$",
    r"(?is)\bPRINT\s+[^;]+?;",
    r"(?is)\bALTER\s+TABLE\s+[a-zA-Z0-9_#$.]+\s+(?:ALTER\s+COLUMN|ADD\s+CONSTRAINT|DROP\s+CONSTRAINT)\s+[\s\S]*?;",
    r"(?is)\b(?:CREATE|UPDATE|DROP)\s+STATISTICS\b[\s\S]*?;",
    r"(?is)\b(?:CREATE|DROP)\s+SCHEMA\s+(?:IF\s+EXISTS\s+)?[a-zA-Z0-9_#$]+(?:\s+AUTHORIZATION\s+[^;]+)?;?",
    r"(?is)\b(?:CREATE|DROP|OPEN|CLOSE)\s+(?:MASTER\s+KEY|CERTIFICATE|SYMMETRIC\s+KEY|ASYMMETRIC\s+KEY)\b[\s\S]*?;",
    r"(?i)\bKey_GUID\s*\([^)]*\)",
    r"(?i)\bEncryptByKey\s*\([^)]*\)",
    r"(?i)\bDecryptByKey\s*\([^)]*\)",
    r"(?is)\b(?:CREATE|DROP)\s+(?:MESSAGE\s+TYPE|CONTRACT|QUEUE|SERVICE)\b[\s\S]*?;",
    r"(?is)\b(?:BEGIN\s+DIALOG(?:\s+CONVERSATION)?|SEND\s+ON\s+CONVERSATION|END\s+CONVERSATION|RECEIVE\b)[\s\S]*?;",
    r"(?is)\bWAITFOR\s*\([\s\S]*?\)\s*,\s*TIMEOUT\s+\d+\s*;?",
    r"(?i)\bEVENTDATA\s*\(\s*\)",
    r"(?is)\b(?:CREATE|DROP)\s+(?:DEFAULT|RULE)\s+[a-zA-Z0-9_#$.]+(?:\s+AS\s+[\s\S]+?)?;",
    r"(?im)^\s*GO\s*;?\s*$",
    r"(?i)(\A|[^a-zA-Z0-9_#$])N'((?:[^']|'')*)'",
    r"(?is)\bIF\s+EXISTS\s*\((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*\)\s*DROP\s+(PARTITION\s+(?:SCHEME|FUNCTION)|SECURITY\s+POLICY|XML\s+SCHEMA\s+COLLECTION|[a-zA-Z0-9_#$]+)\s+([a-zA-Z0-9_#$.\[\]]+);?",
    r"(?is)\bIF\s+EXISTS\s*\((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*\)\s*(?:BEGIN\b)?",
    r"(?is)\bDROP\s+(?:PROCEDURE|PROC|FUNCTION|SYNONYM|SEQUENCE|SECURITY\s+POLICY)\s+(?:IF\s+EXISTS\s+)?([a-zA-Z0-9_#$.]+);?",
    r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?(?:PROCEDURE|PROC)\s+[\s\S]*?\bAS\b\s+BEGIN[\s\S]*?\bEND\b;?",
    r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?FUNCTION\s+[\s\S]*?\bAS\b\s+BEGIN[\s\S]*?\bEND\b;?",
    r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?TRIGGER\s+[\s\S]*?\bAS\b\s+BEGIN[\s\S]*?\bEND\b;?",
    r"(?is)\bEXEC(?:UTE)?\s+[a-zA-Z0-9_#$.]+[\s\S]*?;",
    r"(?i)\(\s*SELECT\s+((?:[^()]+|\([^()]*\))+)\s+FROM\s+((?:[^()]+|\([^()]*\))+)\s+FOR\s+(?:JSON|XML)\s+(?:PATH|AUTO|RAW)(?:\s*,\s*(?:ROOT\s*\([^()]*\)|INCLUDE_NULL_VALUES|WITHOUT_ARRAY_WRAPPER))*\s*\)",
    r"(?i)\bFOR\s+(?:JSON|XML)\s+(?:PATH|AUTO|RAW)(?:\s*,\s*(?:ROOT\s*\([^)]*\)|INCLUDE_NULL_VALUES|WITHOUT_ARRAY_WRAPPER))*",
    r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?VIEW\b",
    r"(?i)\bCREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?",
    r"(?i)\bDROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?",
    r"(?i)\bDROP\s+VIEW\s+(?:IF\s+EXISTS\s+)?",
    r"(?i)\bCREATE\s+(UNIQUE\s+)?INDEX\s+(?:IF\s+NOT\s+EXISTS\s+)?",
    r#"(?i)\bdbo\.(\[[^\]]+\]|"[^"]+"|[a-zA-Z0-9_#$]+)"#,
    r"(?i)\bFOREIGN\s+KEY\s+REFERENCES\b",
    r"(?i)\bPERSISTED\b",
    r"(?is)\bINCLUDE\s*\((?:[^()]*|\([^()]*\))*\)",
    r"(?i)\bWITH\s*\(\s*(?:NOLOCK|READUNCOMMITTED|READCOMMITTED|REPEATABLEREAD|SERIALIZABLE|TABLOCK|TABLOCKX|PAGLOCK|ROWLOCK|UPDLOCK|XLOCK|HOLDLOCK|READPAST|NOWAIT|NOEXPAND|FORCESCAN|FORCESEEK(?:\s*\([^)]*\))?|INDEX\s*\([^)]*\))(?:\s*,\s*(?:NOLOCK|READUNCOMMITTED|READCOMMITTED|REPEATABLEREAD|SERIALIZABLE|TABLOCK|TABLOCKX|PAGLOCK|ROWLOCK|UPDLOCK|XLOCK|HOLDLOCK|READPAST|NOWAIT|NOEXPAND|FORCESCAN|FORCESEEK(?:\s*\([^)]*\))?|INDEX\s*\([^)]*\)))*\s*\)",
    r"(?i)\bDEFAULT\s+CAST\s*\(\s*(?:GETDATE|SYSDATETIME)\(\)\s+AS\s+DATE\s*\)",
    r"(?i)\bDEFAULT\s+GETDATE\(\)",
    r"(?i)\bDEFAULT\s+SYSDATETIME\(\)",
    r"(?i)\bDEFAULT\s+NEWID\(\)",
    r"(?i)\bDEFAULT\s+NEWSEQUENTIALID\(\)",
    r"(?i)\bDEFAULT\s+(uuid_v[47]\(\)|newid\(\))",
    r"(?i)\bDEFAULT\s+NOW\(\)",
    r"(?is)\bDECLARE\s+@([a-zA-Z0-9_]+)\s+TABLE\s*\(((?:[^()]|\([^()]*\))*)\)\s*;?",
    r"(?is)\bSET\s+@[a-zA-Z0-9_#$]+\s*[-+*/%&|^]?=\s*(?:'(?:[^']|'')*'|[^;])*;\s*",
    r"(?i)\bSELECT\s+@[a-zA-Z0-9_#$]+\s*=",
    r"(?is)\bSET\s+(?:@[a-zA-Z0-9_#$]+|[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.modify\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)\s*;?",
    r"(?is)(?:@[a-zA-Z0-9_#$]+|(?:[a-zA-Z0-9_#$]+\.)*[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.exist\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)",
    r"(?is)(?:@[a-zA-Z0-9_#$]+|(?:[a-zA-Z0-9_#$]+\.)*[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.query\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)",
    r"(?is)(?:@[a-zA-Z0-9_#$]+|(?:[a-zA-Z0-9_#$]+\.)*[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.value\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)",
    r"(?is)\b(FROM|JOIN)\s+(?:@[a-zA-Z0-9_#$]+|[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.nodes\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)\s*(?:AS\s+)?([a-zA-Z0-9_#$]+)(?:\s*\([^)]*\))?",
    r"(?i)\bDECLARE\s+@([a-zA-Z0-9_]+)\s+[^=;,\n]+=\s*('(?:[^']|'')*'|[^;,\n]+);?",
    r"(?is)\bOPENXML\s*\([^)]*\)\s+WITH\s*\([^)]*\)",
    r"(?is)\bOUTPUT\s+(?:[^;()]|\([^()]*\))+?\s+INTO\s+@?([a-zA-Z0-9_#$]+)(?:\s*\([^)]*\))?",
    r"(?is)\bOUTPUT\s+(?:(?:inserted|deleted)\.[a-zA-Z0-9_#$]+(?:\s*,\s*(?:inserted|deleted)\.[a-zA-Z0-9_#$]+)*)",
    r"(?is)\bCREATE\s+(?:OR\s+ALTER\s+)?(?:PROCEDURE|PROC|FUNCTION|TRIGGER)\b[\s\S]*?\bAS\b[\s\S]*?\bBEGIN\b[\s\S]*?\bEND\s*;?",
    r"(?is)\bCREATE\s+(?:OR\s+ALTER\s+)?FUNCTION\b[\s\S]*?\bRETURNS\s+TABLE\b[\s\S]*?\bAS\b[\s\S]*?\bRETURN\b[\s\S]*?(?:;|\z)",
    r"(?is)\bCREATE\s+SECURITY\s+POLICY\s+[\s\S]*?(?:;|\z)",
    r"(?is)\bDROP\s+SECURITY\s+POLICY\s+[\s\S]*?(?:;|\z)",
    r"(?is)\bIF\s+SCHEMA_ID\s*\([^)]*\)\s+IS\s+NULL\s+BEGIN[\s\S]*?END;?",
    r"(?is)\bEXEC(?:UTE)?\s*\((?:'(?:[^']|'')*'|[^;])*\)\s*;?",
    r"(?is)\bEXEC(?:UTE)?\s+(?:@[a-zA-Z0-9_#$]+\s*=\s*)?(?:sys\.)?(?:sp_[a-zA-Z0-9_]+|[a-zA-Z0-9_#$.]+)[\s\S]*?;",
    r"(?i)\b(?:BEGIN|END)\s+(?:TRY|CATCH)\b;?",
    r"(?i)\bERROR_NUMBER\s*\(\s*\)",
    r"(?i)\bERROR_SEVERITY\s*\(\s*\)",
    r"(?i)\bERROR_MESSAGE\s*\(\s*\)",
    r"(?i)\bERROR_PROCEDURE\s*\(\s*\)",
    r"(?i)\bERROR_LINE\s*\(\s*\)",
    r"(?i)\bERROR_STATE\s*\(\s*\)",
    r#"(?i)\bINSERT\s+(\[[^\]]+\]|"[^"]+"|[a-zA-Z0-9_#$]+)\s*(\(|\bVALUES\b|\bSELECT\b|\bDEFAULT\b)"#,
    r"(?i)@@VERSION\b",
    r"(?i)@@SERVERNAME\b",
    r"(?i)@@TRANCOUNT\b",
    r"(?i)@@ROWCOUNT\b",
    r"(?i)@@IDENTITY\b",
    r"(?i)@@ERROR\b",
    r"(?i)@@OPTIONS\b",
    r"(?i)@@DATEFIRST\b",
    r"(?i)@@LOCK_TIMEOUT\b",
    r"(?i)@@SPID\b",
    r"(?i)\bSCOPE_IDENTITY\s*\(\s*\)",
    r"(?i)\bIDENT_CURRENT\s*\(\s*([^)]*?)\s*\)",
    r"(?i)\b(COL_LENGTH|APPROX_COUNT_DISTINCT)\s*\(",
    r"(?i)\bSERVERPROPERTY\s*\([^)]*\)",
    r"(?i)\bXACT_STATE\s*\(\s*\)",
    r"(?i)\bNEWSEQUENTIALID\(\)",
    r"(?is)\bTHROW(?:\s+[^;]+)?\s*;?",
    r"(?is)\bRAISERROR\s*\([^;]+\)\s*(?:WITH\s+[a-zA-Z0-9_]+)?\s*;?",
    r"(?is)\bINSERT\s+(?:INTO\s+)?@[a-zA-Z0-9_#$]+[\s\S]*?;",
    r"(?is)\bDECLARE\s+@[a-zA-Z0-9_#$]+(?:\s+[^=;]+)?\s*=\s*'(?:[^']|'')*'\s*;",
    r"(?is)\bDECLARE\s+@[^;]+?;",
    r"(?is)\bSET\s+@[a-zA-Z0-9_#$]+\s*=\s*'(?:[^']|'')*'\s*;",
    r"(?is)\bSET\s+@[^;]+?;",
    r"(?is)\bIF\s+(?:OBJECT_ID\s*\([^)]*\)\s+IS\s+NOT\s+NULL|EXISTS\s*\([^)]*\))\s+BEGIN[\s\S]*?\bEND\s*;?",
    r"(?im)^\s*END\s*(?:\n|\s)*ELSE\s+BEGIN",
    r"(?im)^\s*IF\s+[^;]+?\s+BEGIN[\s\S]*?\bEND(?:\s*ELSE\s*BEGIN[\s\S]*?\bEND)?\s*;?",
    r"(?im)^\s*ELSE\s+BEGIN[\s\S]*?\bEND\s*;?",
    r"(?im)^[ \t]*(?:IF\s+[^;\n]+?\s+)?(?:BREAK|CONTINUE)[ \t]*;?[ \t]*$",
    r"(?im)(;[ \t]*|^[ \t]*(?:--|\x{E004})[^\n]*)\n(\s*)^[ \t]*ELSE[ \t]*$",
    r"(?im)^[ \t]*IF\s+[^;\n]+?[ \t]*$",
    r"(?im)^[ \t]*BEGIN[ \t]*$",
    r"(?im)^[ \t]*END[ \t]*;[ \t]*$",
    r"([a-zA-Z0-9_#$.\)]+)\s*\+\s*('(?:[^']|'')*')",
    r"('(?:[^']|'')*')\s*\+\s*([a-zA-Z0-9_#$.\)]+)",
    r"(TraversedPath|DisplayPath)\s*\+\s*",
    r"(?i)\b(?:hierarchyid|TEXT)::GetRoot\s*\(\s*\)",
    r"(?i)\b(?:hierarchyid|TEXT)::Parse\s*\(([^)]+)\)",
    r"(?i)\b([a-zA-Z0-9_#$]+)\.ToString\s*\(\s*\)",
    r"(?i)\b([a-zA-Z0-9_#$]+)\.GetLevel\s*\(\s*\)",
    r"(?i)\b([a-zA-Z0-9_#$]+)\.IsDescendantOf\s*\(([^)]+)\)\s*=\s*1",
    r"(?is)\bWHILE\s+[\s\S]*?\bBEGIN([\s\S]*?)\bEND\s*;?",
    r"(?i)@[a-zA-Z0-9_#$]+",
    r"(?is),\s*NEWSEQUENTIALID\b",
    r"(?i)\bINFORMATION_SCHEMA\.TABLES\b",
    r"(?i)\bINFORMATION_SCHEMA\.COLUMNS\b",
    r"(?i)\bWITH\s*\(\s*(?:NOLOCK|READUNCOMMITTED|READCOMMITTED|REPEATABLEREAD|SERIALIZABLE|TABLOCK|TABLOCKX|PAGLOCK|ROWLOCK|UPDLOCK|XLOCK|HOLDLOCK|READPAST|NOWAIT)(?:\s*,\s*(?:NOLOCK|READUNCOMMITTED|READCOMMITTED|REPEATABLEREAD|SERIALIZABLE|TABLOCK|TABLOCKX|PAGLOCK|ROWLOCK|UPDLOCK|XLOCK|HOLDLOCK|READPAST|NOWAIT|INDEX\s*\([^)]*\)))*\s*\)",
    r"(?is)\bTABLESAMPLE\s+[a-zA-Z0-9_]+\s*\([^)]*\)(?:\s+REPEATABLE\s*\([^)]*\))?",
    r"(?i)\bIF\s+(?:OBJECT_ID\s*\([^)]*\)\s+IS\s+NOT\s+NULL|EXISTS\s*\([^)]*\))\s+DROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?([a-zA-Z0-9_#$]+);?",
    r"#{1,2}([a-zA-Z0-9_]+)",
    r"\b0x([0-9a-fA-F]{2,})\b",
    r"(?i)\bROWVERSION\b",
    r"(?i)\bROWGUIDCOL\b",
    r"(?is)\bMASKED\s+WITH\s*\(\s*FUNCTION\s*=\s*'[^']+'\s*\)",
    r"(?i)(\bCOLLATE\s+)?\b(?:VARBINARY\b(?:\s*\(\s*(?:MAX|\d+)\s*\))?|BINARY(?:\s*\(\s*(?:MAX|\d+)\s*\)|\b))",
    r"(?i)\b(?:ASCII|UNICODE)\s*\(([^)]+)\)",
    r"(?i)\bNCHAR\s*\(([^)]+)\)",
    r"(?i)\b(?:N?VARCHAR\b(?:\s*\(\s*(?:MAX|\d+)\s*\))?|CHAR\s*\(\s*MAX\s*\))",
    r"(?i)\bXML\b(?:\s*\([^)]*\))?",
    r"(?i)\bVECTOR\b(?:\s*\(\s*\d+\s*\))?",
    r"(?i)\b(?:UNIQUEIDENTIFIER|SQL_VARIANT|HIERARCHYID|GEOMETRY|GEOGRAPHY|JSON|IMAGE|NTEXT|TEXT)\b(\s*\()?",
    r"(?i)\b(?:SMALL)?MONEY\b",
    r"(?i)\b(?:BIT|TINYINT|SMALLINT|BIGINT)\b",
    r"(?i)\b(?:DATETIME2|DATETIMEOFFSET|SMALLDATETIME)\b(?:\s*\(\s*\d+\s*\))?",
    r"(?i)\bCOLLATE\s+((?:SQL_)?(?:Latin1_General|Albanian|Arabic|Chinese|Cyrillic_General|Danish|Finnish|French|German|Greek|Hebrew|Hungarian|Japanese|Korean|Polish|Romanian|Slovak|Thai|Turkish|Ukrainian|Vietnamese)[a-zA-Z0-9_]*)\b",
    r"(?i)\bIS\s+NOT\s+DISTINCT\s+FROM\b",
    r"(?i)\bIS\s+DISTINCT\s+FROM\b",
    r"(?i)\.WRITE\s*\(\s*([^,\n]+)\s*,\s*[^)\n]+\)",
    r"(?is)\bGENERATED\s+ALWAYS\s+AS\s+ROW\s+(?:START|END)(?:\s+HIDDEN)?\b",
    r"(?is),\s*PERIOD\s+FOR\s+SYSTEM_TIME\s*\([^)]*\)",
    r"(?is)\)\s*WITH\s*\((?:[^;()]|\([^()]*\))*\)\s*;?",
    r"(?is)\bALTER\s+TABLE\s+([a-zA-Z0-9_#$]+)\s+SET\s*\(\s*SYSTEM_VERSIONING\s*=\s*(?:ON|OFF)\s*\);?",
    r"(?is)\bFOR\s+SYSTEM_TIME\s+(?:ALL|AS\s+OF\s+[^;\n]+)",
    r"(?i)\bDEFAULT\s+CONVERT\s*\([^,]+,\s*('(?:[^']|'')*')\s*\)",
    r"(?i)\bDEFAULT\s+SYSUTCDATETIME\s*\(\s*\)",
    r"(?i)\b(?:GETDATE|GETUTCDATE|SYSDATETIME|SYSUTCDATETIME|SYSDATETIMEOFFSET)\s*\(\s*\)",
    r"(?is)\bCREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([a-zA-Z0-9_#$]+)\s*\(\s*((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*)\)\s*AS\s+NODE\s*;?",
    r"(?is)\bCREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([a-zA-Z0-9_#$]+)\s*\(\s*((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*)\)\s*AS\s+EDGE\s*;?",
    r"(?i)\bAS\s+(?:NODE|EDGE)\b",
    r"(?is)\bMATCH\s*\(\s*([a-zA-Z0-9_]+)\s*-\s*\(\s*([a-zA-Z0-9_]+)\s*\)\s*->\s*([a-zA-Z0-9_]+)\s*\)",
    r"\$([a-zA-Z0-9_]+)",
    r"(?i)\bSPARSE\s+NULL\b",
    r"(?is)\bCOLUMN_SET\s+FOR\s+ALL_SPARSE_COLUMNS\b",
    r"(?is)\b(?:IF\s+TYPE_ID\s*\([^)]*\)\s+IS\s+NOT\s+NULL\s+)?DROP\s+TYPE\s+[a-zA-Z0-9_#$.]+;?",
    r"(?is)\bCREATE\s+TYPE\s+[a-zA-Z0-9_#$.]+\s+(?:AS\s+TABLE\s*\([^)]*\)|FROM\s+[^;]+);?",
    r"(?i)\bREADONLY\b",
    r"(?is)\b(?:IF\s+EXISTS\s*\([^)]*\)\s+)?DROP\s+PARTITION\s+(?:SCHEME|FUNCTION)\s+[a-zA-Z0-9_#$.]+;?",
    r"(?is)\bCREATE\s+PARTITION\s+FUNCTION\s+[\s\S]+?;",
    r"(?is)\bCREATE\s+PARTITION\s+SCHEME\s+[\s\S]+?;",
    r"(?is)\bON\s+ps_[a-zA-Z0-9_#$]+\s*\([^)]*\);?",
    r"(?i)\$?PARTITION\.[a-zA-Z0-9_#$]+\s*\(([^)]+)\)",
    r"(?i)\b(?:CLUSTERED|NONCLUSTERED)\b",
    r"(?is)\bCREATE\s+(?:CLUSTERED\s+|NONCLUSTERED\s+)?COLUMNSTORE\s+INDEX\s+[\s\S]+?;",
    r"(?i)\bWITH\s+SCHEMABINDING\b",
    r"(?is)\bCREATE\s+(?:UNIQUE\s+)?(?:CLUSTERED\s+|NONCLUSTERED\s+)?INDEX\s+[a-zA-Z0-9_#$]+\s+ON\s+(?:vw_[a-zA-Z0-9_#$]+|view_[a-zA-Z0-9_#$]+)\s*\([^)]*\);?",
    r"(?i)\bWITH\s*\(\s*NOEXPAND\s*\)",
    r"(?is)\bCREATE\s+(?:PRIMARY\s+)?(?:XML|TEXT)\s+INDEX\s+[\s\S]+?;",
    r"(?is)\bCROSS\s+APPLY\s+[a-zA-Z0-9_#$]+\.[a-zA-Z0-9_#$]+\.nodes\s*\([^)]*\)\s+AS\s+[a-zA-Z0-9_#$]+\s*\([^)]*\)",
    r"(?is)\b[a-zA-Z0-9_#$]+\.[a-zA-Z0-9_#$]+\.value\s*\((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*\)",
    r"(?is)\bCREATE\s+SPATIAL\s+INDEX\s+[\s\S]+?;",
    r"(?is)\b(?:APPROX_)?PERCENTILE_(CONT|DISC)\s*\(\s*([^()]+?)\s*\)\s*WITHIN\s+GROUP\s*\(\s*ORDER\s+BY\s+([a-zA-Z0-9_#$.]+)(\s+(?:ASC|DESC))?\s*\)(\s*OVER\s*\(([^)]*)\))?",
    r"(?is)\bPERCENT_RANK\s*\(\s*\)\s*OVER\s*\(([^)]*)\)",
    r"(?i)\bCOUNT_BIG\s*\(\s*\*\s*\)",
    r"(?i)\bCOUNT_BIG\s*\(([^)]+)\)",
    r"(?i)\bJSON_MODIFY\s*\(([^,]+),([^,]+),([^)]+)\)",
    r"(?is)\b(?:IF\s+EXISTS\s*\([^)]*\)\s+)?DROP\s+SECURITY\s+POLICY\s+[a-zA-Z0-9_#$.]+;?",
    r"(?is)\bCREATE\s+SECURITY\s+POLICY\s+[\s\S]+?;",
    r"(?is)\bSTRING_AGG\s*\(((?:[^,()]|\((?:[^()]|\([^()]*\))*\))+),((?:[^()]|\([^()]*\))+)\)(?:\s*WITHIN\s+GROUP\s*\(\s*ORDER\s+BY\s+((?:[^()]|\([^()]*\))+)\))?",
    r"(?i)\bQUOTENAME\s*\(([^)]+)\)",
    r"(?i)\bCHARINDEX\s*\(([^,]+),([^)]+)\)",
    r"(?is)\bDECLARE\s+[a-zA-Z0-9_#$]+\s+CURSOR\b[\s\S]*?;",
    r"(?i)\bOPEN\s+[a-zA-Z0-9_#$]+;?",
    r"(?is)\bFETCH\s+NEXT\s+FROM\s+[a-zA-Z0-9_#$]+(?:\s+INTO\s+[^;]+)?;?",
    r"(?i)\bCLOSE\s+[a-zA-Z0-9_#$]+;?",
    r"(?i)\bDEALLOCATE\s+[a-zA-Z0-9_#$]+;?",
    r"(?i)@@FETCH_STATUS\b",
    r"(?i)\b(?:SMALL|BIG)?SERIAL\b((?:\s+NOT\s+NULL)?\s+PRIMARY\s+KEY\b)?",
    r"(?i)\b(?:INT(?:EGER)?|BIGINT|SMALLINT|TINYINT)((?:\s+NOT\s+NULL)?\s+PRIMARY\s+KEY)?\s+GENERATED\s+(?:ALWAYS|BY\s+DEFAULT)\s+AS\s+IDENTITY(?:\s*\([^)]*\))?((?:\s+NOT\s+NULL)?\s+PRIMARY\s+KEY\b)?",
    r"(?i)\b(?:INT(?:EGER)?|BIGINT|SMALLINT|TINYINT)\s+IDENTITY(?:\s*\(\s*\d+\s*,\s*\d+\s*\))?(?:\s+NOT\s+NULL)?(?:\s+CONSTRAINT\s+[a-zA-Z0-9_#$]+)?\s+PRIMARY\s+KEY\b",
    r"(?i)\bPRIMARY\s+KEY\s+(?:INT(?:EGER)?|BIGINT|SMALLINT|TINYINT)\s+IDENTITY(?:\s*\(\s*\d+\s*,\s*\d+\s*\))?\b",
    r#"(?i)(\[[^\]]+\]|"[^"]+"|[a-zA-Z_][a-zA-Z0-9_]*)\s+(?:INT(?:EGER)?|BIGINT|SMALLINT|TINYINT|DECIMAL\s*\(\s*\d+\s*(?:,\s*0\s*)?\)|NUMERIC\s*\(\s*\d+\s*(?:,\s*0\s*)?\))\s+IDENTITY(?:\s*\(\s*-?\d+\s*,\s*-?\d+\s*\))?"#,
    r"(?i)\bIDENTITY(?:\s*\(\s*-?\d+\s*,\s*-?\d+\s*\))?",
    r"(?i)\bAUTO_INCREMENT\b",
    r"(?i)\b(?:INT|BIGINT|SMALLINT)\s+PRIMARY\s+KEY\s+AUTOINCREMENT\b",
    r"(?i)\bOBJECT_NAME\s*\(\s*((?:[^,()]|\((?:[^()]|\([^()]*\))*\))+?)\s*(?:,[^()]*)?\)",
    r"(?i)\bOBJECT_ID\s*\(\s*((?:[^,()]|\((?:[^()]|\([^()]*\))*\))+?)\s*(?:,[^()]*)?\)",
    r"(?i)\bsys\.(?:partition_functions|partition_schemes)\b",
    r"(?i)\bsys\.(all_objects|objects|tables|views|columns|all_columns|schemas|indexes)\b",
    r"('(?:[^']|'')*')\s*\+\s*",
    r"\s*\+\s*('(?:[^']|'')*')",
    r"(?i)\bISNULL\s*\(",
    r"(?i)\bTRY_CAST\s*\(\s*((?:[^()]+|\([^)]*\))+)\s+AS\s+([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*\)",
    r"(?i)\bTRY_CONVERT\s*\(\s*([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*,\s*(((?:[^(),]|\((?:[^()]|\([^()]*\))*\))*))\s*,\s*\d+\s*\)",
    r"(?i)\bTRY_CONVERT\s*\(\s*([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*,\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s*\)",
    r"(?i)\bCONVERT\s*\(\s*([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*,\s*(((?:[^(),]|\((?:[^()]|\([^()]*\))*\))*))\s*,\s*(\d+)\s*\)",
    r"(?i)\bCONVERT\s*\(\s*([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*,\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s*\)",
    r"(?i)\bCAST\s*\(\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s+AS\s+N?(?:VAR)?CHAR(?:\(\s*(?:\d+|MAX)\s*\))?\s*\)",
    r"(?i)\bCAST\s*\(\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s+AS\s+(DATE|DATETIME2?|SMALLDATETIME|DATETIMEOFFSET|TIME)(?:\s*\(\s*\d+\s*\))?\s*\)",
    r"(?i)\bCAST\s*\(\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s+AS\s+(?:DECIMAL|NUMERIC|MONEY|SMALLMONEY|FLOAT|REAL)(?:\(\s*\d+\s*(?:,\s*\d+\s*)?\))?\s*\)",
    r"(?i)\bCAST\s*\(\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s+AS\s+(?:BIGINT|INT|INTEGER|SMALLINT|TINYINT|BIT)\s*\)",
    r"(?is)\b(FROM|JOIN)\s*\(\s*VALUES\s*(\([^\)]+\)(?:\s*,\s*\([^\)]+\))*)\s*\)\s*(?:AS\s+)?([a-zA-Z0-9_#$]+)\s*\(([^)]+)\)",
    r"(?i)\b([a-zA-Z0-9_#$]+)\s+AS\s*\(\s*SELECT\s+\*\s+FROM\s*\(\s*VALUES\s+([\s\S]*?)\s*\)\s*AS\s+[a-zA-Z0-9_#$]+\s*\(([^)]+)\)\s*\)",
    r"(?i)\bWITH\s+([a-zA-Z0-9_#$]+)\s*(?:\([^)]*\))?\s+AS\b",
    r"(?i)\b(CROSS|OUTER)\s+APPLY\s*(\()\s*SELECT\s+",
    r"(?is)\b(?:CROSS|OUTER)\s+APPLY\s*\(\s*SELECT\s+((?:[^()]|\((?:[^()]|\([^()]*\))*\))*?)\s+AS\s+([a-zA-Z0-9_#$]+)\s*\)\s*(?:AS\s+)?([a-zA-Z0-9_#$]+)",
    r"(?i)\bCROSS\s+APPLY\b",
    r"(?i)\bOUTER\s+APPLY\b",
    r"(?i)\b(DATEADD|DATEDIFF_BIG|DATEDIFF|DATEPART|DATENAME|DATETRUNC|DATE_TRUNC|DATE_PART|DATE_BUCKET)\s*\(\s*([a-zA-Z_]+)\s*,",
    r"(?is)\bOPTION\s*\([^)]*\)",
    r"(?i)\bSELECT\s+TOP\s*\(?\s*(\d+)\s*\)?(?:\s+(?:PERCENT|WITH\s+TIES))*\s+",
    r"(?i)\bOFFSET\s+(\d+)\s+ROWS?\s+FETCH\s+NEXT\s+(\d+)\s+ROWS?\s+ONLY",
    r"(?is)\bWITH\s+(?:RECURSIVE\s+)?([a-zA-Z0-9_#$]+)\s+AS\s*\(\s*SELECT\s+(?:[^;()]|\([^()]*\))+?\bFROM\s+([a-zA-Z0-9_#$]+)(?:\s+WHERE\s+([^;()]+?))?\s*\)\s*UPDATE\s+([a-zA-Z0-9_#$]+)\s+SET\s+([^;]+?);",
    r"(?is)\bWITH\s+(?:RECURSIVE\s+)?([a-zA-Z0-9_#$]+)\s+AS\s*\(\s*SELECT\s+(?:[^;()]|\([^()]*\))+?\bFROM\s+([a-zA-Z0-9_#$]+)(?:\s+WHERE\s+([^;()]+?))?\s*\)\s*DELETE\s+FROM\s+([a-zA-Z0-9_#$]+)\s*;",
    r"(?is)\b([a-zA-Z0-9_#$.]+)\s*(>|>=)\s*(?:ANY|SOME)\s*\(\s*SELECT\s+([a-zA-Z0-9_#$.]+)\s+FROM\s+([^)]+)\)",
    r"(?is)\b([a-zA-Z0-9_#$.]+)\s*(<|<=)\s*(?:ANY|SOME)\s*\(\s*SELECT\s+([a-zA-Z0-9_#$.]+)\s+FROM\s+([^)]+)\)",
    r"(?is)\b([a-zA-Z0-9_#$.]+)\s*=\s*(?:ANY|SOME)\s*\(\s*SELECT\s+",
    r"(?is)\b([a-zA-Z0-9_#$.]+)\s*(>|>=)\s*ALL\s*\(\s*SELECT\s+([a-zA-Z0-9_#$.]+)\s+FROM\s+([^)]+)\)",
    r"(?is)\b([a-zA-Z0-9_#$.]+)\s*(<|<=)\s*ALL\s*\(\s*SELECT\s+([a-zA-Z0-9_#$.]+)\s+FROM\s+([^)]+)\)",
    r"(?is)\bGROUP\s+BY\s+GROUPING\s+SETS\s*\((.+?)\)\s*;",
    r"(?i)\bGROUP\s+BY\s+(?:ROLLUP|CUBE)\s*\(([^)]+)\)",
    r"(?i)\bSELECT\s+([^;]*?)\s+INTO\s+([a-zA-Z0-9_#$]+)\s+FROM\s+",
    r"(?i)\bCREATE\s+SYNONYM\s+([a-zA-Z0-9_#$.]+)\s+FOR\s+([a-zA-Z0-9_#$.]+);?",
    r"(?is)\bSELECT\s+([^;]+?)\s+FROM\s*\(\s*(\bSELECT\b[^;]+?)\s*\)\s*(?:AS\s+)?[a-zA-Z0-9_#$]+\s+PIVOT\s*\(\s*([a-zA-Z0-9_#$]+)\s*\(\s*([a-zA-Z0-9_#$]+)\s*\)\s+FOR\s+([a-zA-Z0-9_#$]+)\s+IN\s*\(\s*(\[[^\]]+\](?:\s*,\s*\[[^\]]+\])*)\s*\)\s*\)\s*(?:AS\s+)?[a-zA-Z0-9_#$]+\s*;?",
    r"(?is)\bSELECT\s+([^;]+?)\s+FROM\s*\(\s*(\bSELECT\b[^;]+?)\s*\)\s*(?:AS\s+)?[a-zA-Z0-9_#$]+\s+UNPIVOT\s*\(\s*([a-zA-Z0-9_#$]+)\s+FOR\s+([a-zA-Z0-9_#$]+)\s+IN\s*\(\s*([a-zA-Z0-9_#$]+(?:\s*,\s*[a-zA-Z0-9_#$]+)*)\s*\)\s*\)\s*(?:AS\s+)?([a-zA-Z0-9_#$]+)(\s*;)?",
    r"(?is)\b(FROM|JOIN)\s+(?:[a-zA-Z0-9_#$]+\.)?(?:fn_|ufn_)[a-zA-Z0-9_#$]*\s*\(((?:[^()]|\([^()]*\))*)\)(?:\s*(?:AS\s+)?([a-zA-Z0-9_#$]+))?",
    r"(?is)\b(?:[a-zA-Z0-9_#$]+\.)?(?:fn_|ufn_)[a-zA-Z0-9_#$]+\s*\(\s*([^)]*)\s*\)",
    r"(?i)(?:[a-zA-Z0-9_#$]+\.)*@?[a-zA-Z0-9_#$]+\.value\s*\([\s\S]*?\)",
    r"(?i)(?:[a-zA-Z0-9_#$]+\.)*@?[a-zA-Z0-9_#$]+\.exist\s*\([\s\S]*?\)",
    r"(?i)(?:[a-zA-Z0-9_#$]+\.)*@?[a-zA-Z0-9_#$]+\.query\s*\([\s\S]*?\)",
    r"(?is)(?:[a-zA-Z0-9_#$]+\.)*@?[a-zA-Z0-9_#$]+\.modify\s*\([\s\S]*?\);?",
    r"(?is)\bFROM\s+STRING_SPLIT\s*\(\s*('(?:[^']|'')*'|[a-zA-Z0-9_#$.]+)\s*,\s*('(?:[^']|'')*'|[a-zA-Z0-9_#$.]+)\s*(?:,\s*([a-zA-Z0-9_#$.]+)\s*)?\)(?:(\s*)(AS\s+)?([a-zA-Z_][a-zA-Z0-9_#$]*))?",
    r"(?is)\bGENERATE_SERIES\s*\(\s*(\d+)\s*,\s*(\d+)(?:\s*,\s*(\d+))?\s*\)(?:\s*(?:AS\s+)?([a-zA-Z0-9_#$]+))?",
    r"(?is)\bFOR\s+JSON\s+(?:PATH|AUTO)(?:\s*,\s*[^;]+)?",
    r"(?is)\bFOR\s+XML\s+(?:PATH|AUTO|RAW|EXPLICIT)(?:\s*\([^)]*\))?(?:\s*,\s*[^;]+)?",
    r"(?i)\bDATALENGTH\s*\(([^)]+)\)",
    r"(?i)\bSPACE\s*\(([^)]+)\)",
    r"(?i)\bSTUFF\s*\(([^,]+),([^,]+),([^,]+),([^)]+)\)",
    r"(?i)\bCURRENT_DATE\b",
    r"(?i)\bDATEFROMPARTS\s*\(([^,]+),([^,]+),([^)]+)\)",
    r"(?i)\bDATETIMEFROMPARTS\s*\(([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),([^)]+)\)",
    r"(?i)\b(?:DATETIME2FROMPARTS|DATETIMEOFFSETFROMPARTS)\s*\(([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),[^)]*\)",
    r"(?i)\bTIMEFROMPARTS\s*\(([^,]+),([^,]+),([^,]+),[^)]*\)",
    r"(?i)\bDAY\s*\(([^)]+)\)",
    r"(?i)\bMONTH\s*\(([^)]+)\)",
    r"(?i)\bYEAR\s*\(([^)]+)\)",
    r"(?i)\bATN2\s*\(",
    r"(?is)\bCOT\s*\(((?:[^()]|\([^()]*\))*)\)",
    r"(?is)\bDEGREES\s*\(((?:[^()]|\([^()]*\))*)\)",
    r"(?is)\bRADIANS\s*\(((?:[^()]|\([^()]*\))*)\)",
    r"(?is)\bLOG10\s*\(((?:[^()]|\([^()]*\))*)\)",
    r"(?i)\bLOG\s*\(\s*([^,\n()]+)\s*,\s*([^,\n()]+)\s*\)",
    r"(?i)\bLOG\s*\(\s*([^,\n()]+)\s*\)",
    r"(?is)\bSIGN\s*\(((?:[^()]|\([^()]*\))*)\)",
    r"(?is)\bSQUARE\s*\(((?:[^()]|\([^()]*\))*)\)",
    r"(?i)\bISJSON\s*\(([^)]+)\)",
    r"(?i)\bJSON_PATH_EXISTS\s*\(([^,]+),([^)]+)\)",
    r"(?i)\bJSON_ARRAYAGG\s*\(([^)]+)\)",
    r"(?i)\bJSON_OBJECTAGG\s*\(([^:]+):([^)]+)\)",
    r"(?i)\bJSON_OBJECT\s*\(([^)]+)\)",
    r"(?i)\bCHOOSE\s*\(\s*(\d+)\s*,\s*([^)]+)\)",
    r"(?i)\bTRY_PARSE\s*\(\s*('(?:[^']|'')*'|[^,()\s]+)\s+AS\s+([a-zA-Z0-9_]+(?:\s*\([^()]*\))?)(?:\s+USING\s+[^)]+)?\)",
    r"(?i)\bPARSE\s*\(\s*('(?:[^']|'')*'|[^,()\s]+)\s+AS\s+[^)]+\)",
    r"(?i)\bFORMAT\s*\(",
    r"(?i)\bGREATEST\s*\(",
    r"(?i)\bLEAST\s*\(",
    r"(?i)\bsys\.database_scoped_configurations\b",
    r"(?i)\bsys\.dm_exec_sessions\b",
    r"(?i)\bsys\.dm_exec_connections\b",
    r"(?i)\bsys\.sequences\b",
    r"(?i)\bsys\.security_policies\b",
    r"(\b[a-zA-Z0-9_#$]+|\d+)\s*\^\s*(\b[a-zA-Z0-9_#$]+|\d+)",
];

static SCREEN: LazyLock<Option<RegexSet>> = LazyLock::new(|| {
    RegexSetBuilder::new(PASS_PATTERNS)
        .dfa_size_limit(64 << 20)
        .build()
        .ok()
});

/// Masked string literals / quoted identifiers / comments are represented by
/// private-use placeholder characters (`U+E000..=U+E003`, digits
/// `U+E010..=U+E01F`). They are neither word characters, digits nor
/// whitespace, exactly like the ASCII control character `U+0001`, but any
/// non-ASCII byte in the haystack forces the regex engine's lazy DFA to give
/// up (Unicode `\b`) and fall back to the slow NFA simulation - ~400 us for a
/// 40-byte statement instead of ~0.1 us. The screen therefore sees the
/// placeholders as `U+0001`; no pattern refers to either character.
fn is_neutral_placeholder(c: char) -> bool {
    matches!(c, '\u{E000}'..='\u{E003}' | '\u{E010}'..='\u{E01F}')
}

/// Returns `true` when no dialect pass can match `text` (all passes are the
/// identity). Returns `false` whenever unsure, e.g. if the set failed to build.
pub(crate) fn no_pass_matches(text: &str) -> bool {
    let Some(set) = SCREEN.as_ref() else {
        return false;
    };
    if text.is_ascii() {
        return !set.is_match(text);
    }
    let ascii: String = text
        .chars()
        .map(|c| {
            if is_neutral_placeholder(c) {
                '\u{1}'
            } else {
                c
            }
        })
        .collect();
    !set.is_match(&ascii)
}

#[cfg(test)]
thread_local! {
    static FORCE_FULL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test hook: lets differential tests run the complete pass pipeline.
#[cfg(test)]
pub(crate) fn set_force_full_pipeline(force: bool) {
    FORCE_FULL.with(|cell| cell.set(force));
}

#[cfg(test)]
pub(crate) fn force_full_pipeline() -> bool {
    FORCE_FULL.with(std::cell::Cell::get)
}

#[cfg(not(test))]
#[inline]
pub(crate) fn force_full_pipeline() -> bool {
    false
}

/// Builds the screen on a background thread so the first statement does not pay
/// for it.
pub(crate) fn warm_up() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("novadb-screen-warmup".into())
            .spawn(|| {
                LazyLock::force(&SCREEN);
            });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extracts the literal pattern arguments of every `cached_regex!(...)`
    /// call inside `normalize_single_batch`.
    fn source_patterns() -> Vec<String> {
        let source = include_str!("lib.rs");
        let start = source.find("fn normalize_single_batch").unwrap();
        let end = start
            + source[start..]
                .find("/// How a guarded batch is executed")
                .unwrap();
        let body = &source[start..end];
        let mut patterns = Vec::new();
        let mut rest = body;
        while let Some(at) = rest.find("cached_regex!(") {
            rest = &rest[at + "cached_regex!(".len()..];
            let trimmed = rest.trim_start();
            let after_r = trimmed
                .strip_prefix('r')
                .expect("literal raw string pattern");
            let hashes = after_r.chars().take_while(|c| *c == '#').count();
            let after_quote = after_r[hashes..].strip_prefix('"').unwrap();
            let terminator = format!("\"{}", "#".repeat(hashes));
            let close = after_quote.find(&terminator).unwrap();
            patterns.push(after_quote[..close].to_owned());
        }
        patterns
    }

    #[test]
    fn pass_patterns_match_source() {
        let mut expected = source_patterns();
        expected.sort();
        expected.dedup();
        let mut actual: Vec<String> = PASS_PATTERNS
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect();
        actual.sort();
        actual.dedup();
        assert_eq!(
            actual, expected,
            "dialect_screen::PASS_PATTERNS drifted from normalize_single_batch; regenerate the list"
        );
    }

    fn candidate_statements() -> Vec<String> {
        let mut texts = vec![
            "SELECT 1".to_owned(),
            "SELECT * FROM t WHERE a = 'x' AND b = 5".to_owned(),
            "INSERT INTO t(a, b) VALUES (1, 'it''s')".to_owned(),
            "UPDATE t SET a = a + 1 WHERE id = 3".to_owned(),
            "DELETE FROM t WHERE id IN (1, 2, 3)".to_owned(),
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)".to_owned(),
            "SELECT a, COUNT(*) FROM t GROUP BY a HAVING COUNT(*) > 1 ORDER BY a LIMIT 5"
                .to_owned(),
            "SELECT TOP 3 * FROM t".to_owned(),
            "SELECT ISNULL(a, 0), GETDATE() FROM t".to_owned(),
            "DECLARE @x INT = 5; SELECT @x".to_owned(),
            "UPDATE a SET x = b.y FROM a JOIN b ON a.id = b.id".to_owned(),
            "SELECT * FROM OPENJSON('[1,2]')".to_owned(),
            "SELECT 2 ^ 3".to_owned(),
            "SELECT /* c */ 1 -- trailing".to_owned(),
            "SELECT 'caf\u{e9}', \"\u{e9}t\u{e9}\" FROM t".to_owned(),
        ];
        let mut files: Vec<String> = [
            include_str!("../test_fixtures/test_advanced_full.py"),
            include_str!("../test_fixtures/test_conformance_2025.py"),
            include_str!("../test_fixtures/test_ultra_advanced.py"),
        ]
        .iter()
        .map(|text| (*text).to_owned())
        .collect();
        // Optional extra corpus from the repository's own SQL test scripts.
        let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
        if let Ok(entries) = std::fs::read_dir(tests) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|ext| ext == "py") {
                    if let Ok(text) = std::fs::read_to_string(entry.path()) {
                        files.push(text);
                    }
                }
            }
        }
        for file in &files {
            // Every line, and every quoted / triple-quoted fragment.
            texts.extend(file.lines().map(|line| line.trim().to_owned()));
            for delimiter in ["\"\"\"", "'''", "\"", "'"] {
                let mut parts = file.split(delimiter);
                parts.next();
                texts.extend(parts.step_by(2).map(str::to_owned));
            }
        }
        texts.retain(|text| !text.trim().is_empty() && text.len() < 20_000);
        texts.sort();
        texts.dedup();
        texts
    }

    /// The screened fast path must be indistinguishable from running every pass.
    #[test]
    fn fast_path_equals_full_pipeline_on_corpus() {
        let texts = candidate_statements();
        assert!(
            texts.len() > 500,
            "corpus unexpectedly small: {}",
            texts.len()
        );
        let mut fast_paths = 0_usize;
        for text in &texts {
            set_force_full_pipeline(true);
            let full = crate::normalize_sql_dialect(text);
            set_force_full_pipeline(false);
            let fast = crate::normalize_sql_dialect(text);
            match (&full, &fast) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "fast path diverged for {text:?}"),
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "for {text:?}"),
                _ => panic!("fast path changed success/failure for {text:?}: {full:?} vs {fast:?}"),
            }
            fast_paths += usize::from(crate::normalize_sql_dialect(text).is_ok());
        }
        assert!(fast_paths > 100);
    }

    #[test]
    fn plain_sql_is_screened_out_and_dialect_sql_is_not() {
        assert!(no_pass_matches("SELECT id, name FROM users WHERE id = 5"));
        assert!(no_pass_matches("UPDATE t SET v = v + 1 WHERE id = 7"));
        assert!(!no_pass_matches("SELECT TOP 5 * FROM users"));
        assert!(no_pass_matches(
            "INSERT INTO t VALUES (1, '\u{E000}\u{E010}\u{E001}')"
        ));
        assert!(!no_pass_matches("SELECT GETDATE()"));
        assert!(!no_pass_matches(
            "SELECT TOP 5 \u{E000}\u{E010}\u{E001} FROM t"
        ));
        assert!(!no_pass_matches("SELECT ISNULL(a, 0) FROM t"));
    }
}
