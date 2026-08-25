import requests, re

conformance_script = """/*
===============================================================================
 NOVADB <-> MICROSOFT SQL SERVER 2025 (17.x) SURFACE-CONFORMANCE MASTER SUITE
 Target: SQL Server 2025 Database Engine, compatibility level 170
 Purpose: exercise T-SQL syntax from basic to advanced / SQL Server-specific.
===============================================================================
*/

USE master;
GO

/* ============================================================================
   000 - DATABASE / BATCH / COMPATIBILITY
============================================================================ */

IF DB_ID(N'NovaConformance2025') IS NOT NULL
BEGIN
    ALTER DATABASE NovaConformance2025 SET SINGLE_USER WITH ROLLBACK IMMEDIATE;
    DROP DATABASE NovaConformance2025;
END;
GO

CREATE DATABASE NovaConformance2025;
GO

ALTER DATABASE NovaConformance2025 SET COMPATIBILITY_LEVEL = 170;
GO

USE NovaConformance2025;
GO

SELECT
    @@VERSION AS SqlServerVersion,
    DB_NAME() AS CurrentDatabase,
    DATABASEPROPERTYEX(DB_NAME(), 'Collation') AS DatabaseCollation,
    SERVERPROPERTY('ProductVersion') AS ProductVersion,
    SERVERPROPERTY('ProductLevel') AS ProductLevel,
    SERVERPROPERTY('Edition') AS Edition;
GO

/* TEST 001 - comments, semicolon, quoted/bracket identifiers */
SET QUOTED_IDENTIFIER ON;
GO
CREATE TABLE dbo.[Odd Name]
(
    [select] INT NOT NULL,
    "quoted column" NVARCHAR(20) NULL
);
INSERT dbo.[Odd Name] ([select], "quoted column") VALUES (1, N'ok');
SELECT * FROM dbo.[Odd Name];
DROP TABLE dbo.[Odd Name];
GO

/* ============================================================================
   010 - SESSION SET STATEMENTS
============================================================================ */

SET NOCOUNT ON;
SET ANSI_NULLS ON;
SET ANSI_PADDING ON;
SET ANSI_WARNINGS ON;
SET ARITHABORT ON;
SET CONCAT_NULL_YIELDS_NULL ON;
SET NUMERIC_ROUNDABORT OFF;
SET QUOTED_IDENTIFIER ON;
SET XACT_ABORT OFF;
SET IMPLICIT_TRANSACTIONS OFF;
SET DATEFIRST 1;
SET DATEFORMAT ymd;
SET LOCK_TIMEOUT 5000;
SET DEADLOCK_PRIORITY NORMAL;
SET TEXTSIZE 2147483647;
SET ROWCOUNT 0;
SET TRANSACTION ISOLATION LEVEL READ COMMITTED;
GO

SELECT
    @@OPTIONS AS SessionOptions,
    @@DATEFIRST AS DateFirst,
    @@LOCK_TIMEOUT AS LockTimeout;
GO

/* ============================================================================
   020 - DATA TYPES
============================================================================ */

CREATE TABLE dbo.TypeMatrix
(
    Id INT IDENTITY(1,1) PRIMARY KEY,

    -- exact numerics
    c_bit BIT,
    c_tinyint TINYINT,
    c_smallint SMALLINT,
    c_int INT,
    c_bigint BIGINT,
    c_decimal DECIMAL(38,10),
    c_numeric NUMERIC(28,8),
    c_money MONEY,
    c_smallmoney SMALLMONEY,

    -- approximate numerics
    c_real REAL,
    c_float FLOAT(53),

    -- date/time
    c_date DATE,
    c_time TIME(7),
    c_smalldatetime SMALLDATETIME,
    c_datetime DATETIME,
    c_datetime2 DATETIME2(7),
    c_datetimeoffset DATETIMEOFFSET(7),

    -- character
    c_char CHAR(10),
    c_varchar VARCHAR(200),
    c_varchar_max VARCHAR(MAX),
    c_nchar NCHAR(10),
    c_nvarchar NVARCHAR(200),
    c_nvarchar_max NVARCHAR(MAX),

    -- binary
    c_binary BINARY(8),
    c_varbinary VARBINARY(200),
    c_varbinary_max VARBINARY(MAX),

    -- SQL Server-specific / special
    c_uniqueidentifier UNIQUEIDENTIFIER,
    c_xml XML,
    c_sql_variant SQL_VARIANT,
    c_hierarchyid HIERARCHYID,
    c_geometry GEOMETRY,
    c_geography GEOGRAPHY,

    -- legacy/deprecated but still important for compatibility surface
    c_text TEXT NULL,
    c_ntext NTEXT NULL,
    c_image IMAGE NULL,

    -- timestamp is rowversion synonym
    c_timestamp TIMESTAMP
);
GO

INSERT dbo.TypeMatrix
(
    c_bit,c_tinyint,c_smallint,c_int,c_bigint,c_decimal,c_numeric,c_money,c_smallmoney,
    c_real,c_float,c_date,c_time,c_smalldatetime,c_datetime,c_datetime2,c_datetimeoffset,
    c_char,c_varchar,c_varchar_max,c_nchar,c_nvarchar,c_nvarchar_max,
    c_binary,c_varbinary,c_varbinary_max,c_uniqueidentifier,c_xml,c_sql_variant,
    c_hierarchyid,c_geometry,c_geography
)
VALUES
(
    1,255,32767,2147483647,9223372036854775807,
    12345678901234567890.1234567890,
    12345678901234567890.12345678,
    1234.56,123.45,
    1.25,3.141592653589793,
    '2026-08-25','11:31:12.1234567','2026-08-25T11:31:00',
    '2026-08-25T11:31:12.123','2026-08-25T11:31:12.1234567',
    '2026-08-25T11:31:12.1234567+07:00',
    'abc','varchar',REPLICATE('x',50),N'abc',N'Việt Nam',N'Unicode',
    0x0102030405060708,0x010203,0xAABBCC,
    NEWID(),N'<root><x id="1">Nova</x></root>',CAST(123 AS INT),
    HIERARCHYID::Parse('/1/2/'),
    GEOMETRY::Point(10,20,0),
    GEOGRAPHY::Point(10,20,4326)
);
GO

SELECT * FROM dbo.TypeMatrix;
GO

/* SQL Server 2025 VECTOR data type */
CREATE TABLE dbo.VectorTypeTest
(
    Id INT PRIMARY KEY,
    Embedding VECTOR(3)
);
INSERT dbo.VectorTypeTest VALUES (1, '[0.1, 0.2, 0.3]');
SELECT Id, Embedding FROM dbo.VectorTypeTest;
GO

/* SQL Server 2025 native JSON type.
   On builds where native JSON is preview-gated, enable PREVIEW_FEATURES first. */
BEGIN TRY
    EXEC(N'CREATE TABLE dbo.NativeJsonTypeTest
    (
        Id INT PRIMARY KEY,
        Payload JSON
    );');
    EXEC(N'INSERT dbo.NativeJsonTypeTest VALUES
        (1, ''{"name":"Nova","tags":["sql","2025"]}'');');
    EXEC(N'SELECT * FROM dbo.NativeJsonTypeTest;');
END TRY
BEGIN CATCH
    SELECT ERROR_NUMBER() AS ErrorNumber, ERROR_MESSAGE() AS NativeJsonTypeError;
END CATCH;
GO

/* ============================================================================
   030 - USER-DEFINED TYPES / TABLE TYPES / XML SCHEMA COLLECTION
============================================================================ */

CREATE TYPE dbo.OrderCode FROM VARCHAR(30) NOT NULL;
GO

CREATE TYPE dbo.IntIdList AS TABLE
(
    Id INT NOT NULL PRIMARY KEY
);
GO

CREATE XML SCHEMA COLLECTION dbo.NovaXmlSchema AS
N'<?xml version="1.0"?>
<xsd:schema xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <xsd:element name="root" type="xsd:string"/>
</xsd:schema>';
GO

CREATE TABLE dbo.TypedXmlTest
(
    Id INT PRIMARY KEY,
    Doc XML(dbo.NovaXmlSchema)
);
INSERT dbo.TypedXmlTest VALUES (1,N'<root>Nova</root>');
SELECT * FROM dbo.TypedXmlTest;
GO

/* ============================================================================
   040 - TABLE DDL: IDENTITY / DEFAULT / CHECK / UNIQUE / COMPUTED / ROWVERSION
============================================================================ */

CREATE TABLE dbo.ParentEntity
(
    ParentId INT IDENTITY(100,5) NOT NULL
        CONSTRAINT PK_ParentEntity PRIMARY KEY,
    Code VARCHAR(20) NOT NULL
        CONSTRAINT UQ_ParentEntity_Code UNIQUE,
    Name NVARCHAR(100) NOT NULL,
    CreatedAt DATETIME2(7) NOT NULL
        CONSTRAINT DF_ParentEntity_CreatedAt DEFAULT SYSUTCDATETIME(),
    Amount DECIMAL(18,2) NOT NULL
        CONSTRAINT CK_ParentEntity_Amount CHECK (Amount >= 0),
    AmountWithTax AS (Amount * CONVERT(DECIMAL(18,2),1.10)) PERSISTED,
    RowVer ROWVERSION
);
GO

/* composite PK + composite FK */
CREATE TABLE dbo.CompositeParent
(
    TenantId INT NOT NULL,
    ObjectId INT NOT NULL,
    Name NVARCHAR(100) NOT NULL,
    CONSTRAINT PK_CompositeParent PRIMARY KEY (TenantId,ObjectId),
    CONSTRAINT UQ_CompositeParent UNIQUE (TenantId,Name)
);
GO

CREATE TABLE dbo.CompositeChild
(
    TenantId INT NOT NULL,
    ObjectId INT NOT NULL,
    LineNo INT NOT NULL,
    Qty INT NOT NULL CHECK (Qty > 0),
    CONSTRAINT PK_CompositeChild PRIMARY KEY (TenantId,ObjectId,LineNo),
    CONSTRAINT FK_CompositeChild_Parent
        FOREIGN KEY (TenantId,ObjectId)
        REFERENCES dbo.CompositeParent(TenantId,ObjectId)
        ON UPDATE NO ACTION
        ON DELETE CASCADE
);
GO

/* self-reference */
CREATE TABLE dbo.TreeNode
(
    NodeId INT IDENTITY PRIMARY KEY,
    ParentNodeId INT NULL,
    NodeName NVARCHAR(100) NOT NULL,
    CONSTRAINT FK_TreeNode_Parent
        FOREIGN KEY (ParentNodeId) REFERENCES dbo.TreeNode(NodeId)
);
GO

/* sparse / column set */
CREATE TABLE dbo.SparseTest
(
    Id INT PRIMARY KEY,
    Phone VARCHAR(30) SPARSE NULL,
    Github VARCHAR(200) SPARSE NULL,
    Score DECIMAL(18,2) SPARSE NULL,
    SparseSet XML COLUMN_SET FOR ALL_SPARSE_COLUMNS
);
GO

/* ROWGUIDCOL */
CREATE TABLE dbo.RowGuidTest
(
    Id UNIQUEIDENTIFIER ROWGUIDCOL NOT NULL
        CONSTRAINT DF_RowGuidTest_Id DEFAULT NEWSEQUENTIALID()
        CONSTRAINT PK_RowGuidTest PRIMARY KEY,
    Name NVARCHAR(100)
);
GO

/* ALTER TABLE */
ALTER TABLE dbo.ParentEntity ADD Notes NVARCHAR(500) NULL;
ALTER TABLE dbo.ParentEntity ALTER COLUMN Notes NVARCHAR(1000) NULL;
ALTER TABLE dbo.ParentEntity ADD CONSTRAINT CK_ParentEntity_Notes CHECK (LEN(Notes) <= 1000);
ALTER TABLE dbo.ParentEntity DROP CONSTRAINT CK_ParentEntity_Notes;
ALTER TABLE dbo.ParentEntity DROP COLUMN Notes;
GO

/* ============================================================================
   050 - SEQUENCE / IDENTITY BEHAVIOR
============================================================================ */

CREATE SEQUENCE dbo.NovaSequence
AS BIGINT
START WITH 1000
INCREMENT BY 10
MINVALUE 1000
MAXVALUE 999999999
CYCLE
CACHE 25;
GO

SELECT NEXT VALUE FOR dbo.NovaSequence AS Seq1;
SELECT NEXT VALUE FOR dbo.NovaSequence AS Seq2;
GO

CREATE TABLE dbo.IdentityTest
(
    Id INT IDENTITY(1,1) PRIMARY KEY,
    Value NVARCHAR(50)
);
INSERT dbo.IdentityTest(Value) VALUES(N'A'),(N'B');
SELECT
    SCOPE_IDENTITY() AS ScopeIdentity,
    @@IDENTITY AS AtAtIdentity,
    IDENT_CURRENT('dbo.IdentityTest') AS IdentCurrent;
GO

SET IDENTITY_INSERT dbo.IdentityTest ON;
INSERT dbo.IdentityTest(Id,Value) VALUES(100,N'forced');
SET IDENTITY_INSERT dbo.IdentityTest OFF;
GO

DBCC CHECKIDENT ('dbo.IdentityTest', NORESEED);
GO

/* ============================================================================
   060 - DML: INSERT / OUTPUT / UPDATE / DELETE / MERGE
============================================================================ */

CREATE TABLE dbo.DmlTarget
(
    Id INT IDENTITY PRIMARY KEY,
    Code VARCHAR(20) UNIQUE,
    Qty INT NOT NULL DEFAULT 0,
    Price DECIMAL(18,2) NOT NULL DEFAULT 0,
    Note NVARCHAR(200),
    Payload VARBINARY(MAX)
);
GO

/* DEFAULT VALUES */
INSERT dbo.DmlTarget DEFAULT VALUES;
GO

/* multi-values */
INSERT dbo.DmlTarget(Code,Qty,Price,Note)
VALUES
('A',10,100,N'alpha'),
('B',20,200,N'beta'),
('C',30,300,N'gamma');
GO

/* INSERT SELECT */
INSERT dbo.DmlTarget(Code,Qty,Price)
SELECT 'D',40,400
WHERE NOT EXISTS (SELECT 1 FROM dbo.DmlTarget WHERE Code='D');
GO

/* OUTPUT */
DECLARE @Inserted TABLE(Id INT,Code VARCHAR(20));
INSERT dbo.DmlTarget(Code,Qty,Price)
OUTPUT inserted.Id,inserted.Code INTO @Inserted
VALUES('E',50,500);
SELECT * FROM @Inserted;
GO

/* UPDATE alias FROM */
UPDATE T
SET T.Qty = T.Qty + X.Delta
FROM dbo.DmlTarget AS T
CROSS APPLY (SELECT 5 AS Delta) AS X
WHERE T.Code IN ('A','B');
GO

/* .WRITE() on varbinary(max) */
UPDATE dbo.DmlTarget
SET Payload = 0x0102030405
WHERE Code='A';

UPDATE dbo.DmlTarget
SET Payload .WRITE(0xAABB,1,2)
WHERE Code='A';
GO

/* DELETE with FROM */
BEGIN TRANSACTION;
DELETE T
OUTPUT deleted.Id,deleted.Code
FROM dbo.DmlTarget AS T
WHERE T.Code='C';
ROLLBACK;
GO

/* MERGE + action + inserted/deleted */
DECLARE @MergeSource TABLE
(
    Code VARCHAR(20) PRIMARY KEY,
    Qty INT,
    Price DECIMAL(18,2)
);
INSERT @MergeSource VALUES('A',111,111.11),('Z',999,999.99);

MERGE dbo.DmlTarget AS T
USING @MergeSource AS S
ON T.Code=S.Code
WHEN MATCHED THEN
    UPDATE SET Qty=S.Qty,Price=S.Price
WHEN NOT MATCHED BY TARGET THEN
    INSERT(Code,Qty,Price) VALUES(S.Code,S.Qty,S.Price)
WHEN NOT MATCHED BY SOURCE AND T.Code='E' THEN
    DELETE
OUTPUT
    $action AS MergeAction,
    deleted.Code AS OldCode,
    inserted.Code AS NewCode,
    deleted.Qty AS OldQty,
    inserted.Qty AS NewQty;
GO

/* ============================================================================
   070 - SELECT / FROM / JOIN / APPLY / TABLESAMPLE / TOP / OFFSET
============================================================================ */

SELECT 1 AS One;
SELECT ALL 1 AS One;
SELECT DISTINCT Code FROM dbo.DmlTarget;
SELECT TOP (2) WITH TIES * FROM dbo.DmlTarget ORDER BY Qty DESC;
SELECT TOP (50) PERCENT * FROM dbo.DmlTarget ORDER BY Id;
GO

SELECT *
FROM dbo.DmlTarget
ORDER BY Id
OFFSET 1 ROWS FETCH NEXT 3 ROWS ONLY;
GO

SELECT A.Id,A.Code,B.Code AS OtherCode
FROM dbo.DmlTarget AS A
INNER JOIN dbo.DmlTarget AS B ON B.Id=A.Id;

SELECT A.Id,A.Code,B.Code
FROM dbo.DmlTarget AS A
LEFT OUTER JOIN dbo.DmlTarget AS B ON B.Id=A.Id+1;

SELECT A.Id,A.Code,B.Code
FROM dbo.DmlTarget AS A
RIGHT OUTER JOIN dbo.DmlTarget AS B ON B.Id=A.Id+1;

SELECT A.Id,A.Code,B.Code
FROM dbo.DmlTarget AS A
FULL OUTER JOIN dbo.DmlTarget AS B ON B.Id=A.Id+100;

SELECT TOP (10) A.Code,B.Code
FROM dbo.DmlTarget AS A
CROSS JOIN dbo.DmlTarget AS B;
GO

SELECT T.Code,X.DoubleQty
FROM dbo.DmlTarget AS T
CROSS APPLY (SELECT T.Qty*2 AS DoubleQty) AS X;
GO

SELECT T.Code,X.NextCode
FROM dbo.DmlTarget AS T
OUTER APPLY
(
    SELECT TOP(1) T2.Code AS NextCode
    FROM dbo.DmlTarget AS T2
    WHERE T2.Id>T.Id
    ORDER BY T2.Id
) AS X;
GO

SELECT *
FROM dbo.DmlTarget TABLESAMPLE SYSTEM (50 PERCENT)
REPEATABLE (12345);
GO

/* derived table + scalar subquery + correlated subquery */
SELECT
    X.Code,
    X.Qty,
    (SELECT MAX(Price) FROM dbo.DmlTarget) AS MaxPrice,
    (SELECT COUNT(*) FROM dbo.DmlTarget T2 WHERE T2.Qty>X.Qty) AS HigherQtyRows
FROM
(
    SELECT Code,Qty FROM dbo.DmlTarget WHERE Code IS NOT NULL
) AS X;
GO

/* EXISTS / NOT EXISTS / IN / NOT IN / ANY / ALL / SOME */
SELECT * FROM dbo.DmlTarget T
WHERE EXISTS (SELECT 1 FROM dbo.DmlTarget X WHERE X.Id=T.Id);

SELECT * FROM dbo.DmlTarget
WHERE Id IN (SELECT Id FROM dbo.DmlTarget WHERE Qty>0);

SELECT * FROM dbo.DmlTarget
WHERE Qty > ANY (SELECT Qty FROM dbo.DmlTarget WHERE Qty IS NOT NULL);

SELECT * FROM dbo.DmlTarget
WHERE Qty >= ALL (SELECT Qty FROM dbo.DmlTarget WHERE Qty IS NOT NULL);

SELECT * FROM dbo.DmlTarget
WHERE Qty > SOME (SELECT Qty FROM dbo.DmlTarget WHERE Qty IS NOT NULL);
GO

/* ============================================================================
   080 - SET OPERATORS
============================================================================ */

SELECT 1 AS X UNION SELECT 1 UNION SELECT 2;
SELECT 1 AS X UNION ALL SELECT 1;
SELECT 1 AS X INTERSECT SELECT 1;
SELECT 1 AS X EXCEPT SELECT 2;
GO

/* ============================================================================
   090 - GROUPING / AGGREGATES / PIVOT / UNPIVOT
============================================================================ */

SELECT
    Code,
    COUNT(*) AS Cnt,
    COUNT_BIG(*) AS BigCnt,
    SUM(Qty) AS SumQty,
    AVG(CONVERT(DECIMAL(18,2),Qty)) AS AvgQty,
    MIN(Qty) AS MinQty,
    MAX(Qty) AS MaxQty,
    STDEV(CONVERT(FLOAT,Qty)) AS StdDev,
    STDEVP(CONVERT(FLOAT,Qty)) AS StdDevP,
    VAR(CONVERT(FLOAT,Qty)) AS Variance,
    VARP(CONVERT(FLOAT,Qty)) AS VarianceP,
    CHECKSUM_AGG(CHECKSUM(Qty)) AS ChecksumAgg
FROM dbo.DmlTarget
GROUP BY Code
HAVING COUNT(*)>=1;
GO

SELECT Code,SUM(Qty) AS Qty
FROM dbo.DmlTarget
GROUP BY ROLLUP(Code);
GO

SELECT Code,SUM(Qty) AS Qty
FROM dbo.DmlTarget
GROUP BY CUBE(Code);
GO

SELECT Code,SUM(Qty) AS Qty,GROUPING(Code) AS G,GROUPING_ID(Code) AS Gid
FROM dbo.DmlTarget
GROUP BY GROUPING SETS ((Code),());
GO

SELECT *
FROM
(
    SELECT Code,Qty FROM dbo.DmlTarget WHERE Code IN ('A','B','D')
) S
PIVOT
(
    SUM(Qty) FOR Code IN ([A],[B],[D])
) P;
GO

SELECT Metric,Value
FROM
(
    SELECT CAST(10 AS INT) A,CAST(20 AS INT) B,CAST(30 AS INT) C
) S
UNPIVOT
(
    Value FOR Metric IN(A,B,C)
) U;
GO

/* ============================================================================
   100 - CTE / RECURSIVE CTE / MULTIPLE CTE
============================================================================ */

WITH A AS
(
    SELECT Id,Code,Qty FROM dbo.DmlTarget
),
B AS
(
    SELECT * FROM A WHERE Qty>0
)
SELECT * FROM B;
GO

WITH N AS
(
    SELECT 1 AS n
    UNION ALL
    SELECT n+1 FROM N WHERE n<20
)
SELECT SUM(n) AS RecursiveSum
FROM N
OPTION(MAXRECURSION 100);
GO

/* recursive tree */
INSERT dbo.TreeNode(ParentNodeId,NodeName) VALUES(NULL,N'Root');
DECLARE @Root INT=SCOPE_IDENTITY();
INSERT dbo.TreeNode(ParentNodeId,NodeName) VALUES(@Root,N'Child A'),(@Root,N'Child B');
GO

WITH Tree AS
(
    SELECT NodeId,ParentNodeId,NodeName,0 AS Depth,
           CAST(NodeName AS NVARCHAR(MAX)) AS PathText
    FROM dbo.TreeNode
    WHERE ParentNodeId IS NULL

    UNION ALL

    SELECT C.NodeId,C.ParentNodeId,C.NodeName,P.Depth+1,
           CAST(P.PathText+N' > '+C.NodeName AS NVARCHAR(MAX))
    FROM dbo.TreeNode C
    JOIN Tree P ON C.ParentNodeId=P.NodeId
)
SELECT * FROM Tree
OPTION(MAXRECURSION 32767);
GO

/* ============================================================================
   110 - WINDOW / ANALYTIC
============================================================================ */

SELECT
    Id,Code,Qty,Price,
    ROW_NUMBER() OVER(ORDER BY Qty DESC,Id) AS rn,
    RANK() OVER(ORDER BY Qty DESC) AS rnk,
    DENSE_RANK() OVER(ORDER BY Qty DESC) AS drnk,
    NTILE(4) OVER(ORDER BY Qty) AS quartile,
    LAG(Qty,1,0) OVER(ORDER BY Id) AS prev_qty,
    LEAD(Qty,1,0) OVER(ORDER BY Id) AS next_qty,
    FIRST_VALUE(Qty) OVER(ORDER BY Id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) AS first_qty,
    LAST_VALUE(Qty) OVER(ORDER BY Id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) AS last_qty,
    SUM(Qty) OVER() AS total_qty,
    SUM(Qty) OVER(ORDER BY Id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running_qty,
    AVG(CONVERT(DECIMAL(18,2),Qty)) OVER(ORDER BY Id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) AS moving_avg,
    PERCENT_RANK() OVER(ORDER BY Qty) AS percent_rank,
    CUME_DIST() OVER(ORDER BY Qty) AS cume_dist
FROM dbo.DmlTarget;
GO

SELECT
    Id,Qty,
    PERCENTILE_CONT(0.5) WITHIN GROUP(ORDER BY Qty) OVER() AS median_cont,
    PERCENTILE_DISC(0.5) WITHIN GROUP(ORDER BY Qty) OVER() AS median_disc
FROM dbo.DmlTarget
WHERE Qty IS NOT NULL;
GO

/* SQL Server 2022 approximate percentile */
SELECT
    APPROX_PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY Qty) AS approx_median_cont,
    APPROX_PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY Qty) AS approx_median_disc
FROM dbo.DmlTarget
WHERE Qty IS NOT NULL;
GO

/* named WINDOW - newer syntax */
SELECT
    Id,Code,Qty,
    ROW_NUMBER() OVER W AS rn,
    SUM(Qty) OVER W AS running_qty
FROM dbo.DmlTarget
WINDOW W AS
(
    ORDER BY Id
    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
);
GO

/* ============================================================================
   120 - EXPRESSIONS / OPERATORS / NULL / COLLATION
============================================================================ */

SELECT
    +10 AS UnaryPlus,
    -10 AS UnaryMinus,
    7+3 AS AddValue,
    7-3 AS SubValue,
    7*3 AS MulValue,
    7/3 AS IntDivValue,
    7%3 AS ModValue,
    5&3 AS BitAnd,
    5|3 AS BitOr,
    5^3 AS BitXor,
    ~5 AS BitNot;
GO

SELECT
    CASE WHEN 10>5 THEN 'yes' ELSE 'no' END AS SearchedCase,
    CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'other' END AS SimpleCase,
    IIF(10>5,'yes','no') AS IifResult,
    CHOOSE(2,'one','two','three') AS ChooseResult,
    COALESCE(NULL,NULL,123) AS CoalesceResult,
    ISNULL(NULL,456) AS IsNullResult,
    NULLIF(10,10) AS NullIfResult;
GO

SELECT
    CASE WHEN NULL IS NULL THEN 1 ELSE 0 END AS IsNullTest,
    CASE WHEN NULL IS NOT NULL THEN 1 ELSE 0 END AS IsNotNullTest,
    CASE WHEN 1 BETWEEN 1 AND 2 THEN 1 ELSE 0 END AS BetweenTest,
    CASE WHEN 'abc' LIKE 'a%' THEN 1 ELSE 0 END AS LikeTest,
    CASE WHEN 'abc' LIKE '[a-c]%' THEN 1 ELSE 0 END AS LikeBracketTest;
GO

/* SQL Server 2022 IS [NOT] DISTINCT FROM */
SELECT
    CASE WHEN NULL IS DISTINCT FROM 1 THEN 1 ELSE 0 END AS DistinctFrom,
    CASE WHEN NULL IS NOT DISTINCT FROM NULL THEN 1 ELSE 0 END AS NotDistinctFrom;
GO

SELECT
    N'a' COLLATE Latin1_General_100_CI_AS AS CaseInsensitive,
    N'a' COLLATE Latin1_General_100_BIN2 AS BinaryCollation;
GO

/* ============================================================================
   130 - CONVERSION / FORMAT / PARSE
============================================================================ */

SELECT
    CAST('123' AS INT) AS CastInt,
    CONVERT(VARCHAR(10),CAST('2026-08-25' AS DATE),23) AS ConvertDate,
    TRY_CAST('bad' AS INT) AS TryCastBad,
    TRY_CONVERT(DATE,'2026-08-25') AS TryConvertDate,
    PARSE('25/08/2026' AS DATE USING 'vi-VN') AS ParseDate,
    TRY_PARSE('not-a-date' AS DATE USING 'en-US') AS TryParseBad,
    FORMAT(CAST('2026-08-25' AS DATE),'yyyy-MM-dd','en-US') AS FormatDate;
GO

/* ============================================================================
   140 - STRING FUNCTIONS
============================================================================ */

SELECT
    ASCII('A') AS AsciiA,
    CHAR(65) AS CharA,
    NCHAR(0x20AC) AS Euro,
    UNICODE(N'Đ') AS UnicodeD,
    LEN(N'Nova SQL') AS LenValue,
    DATALENGTH(N'Nova SQL') AS DataLengthValue,
    LOWER(N'NOVA') AS LowerValue,
    UPPER(N'nova') AS UpperValue,
    LTRIM('   x') AS LtrimValue,
    RTRIM('x   ') AS RtrimValue,
    TRIM('   x   ') AS TrimValue,
    LEFT('abcdef',3) AS LeftValue,
    RIGHT('abcdef',3) AS RightValue,
    SUBSTRING('abcdef',2,3) AS SubstringValue,
    CHARINDEX('cd','abcdef') AS CharIndexValue,
    PATINDEX('%cd%','abcdef') AS PatIndexValue,
    REPLACE('abcabc','a','x') AS ReplaceValue,
    REPLICATE('ab',3) AS ReplicateValue,
    REVERSE('abc') AS ReverseValue,
    SPACE(3) AS SpaceValue,
    STUFF('abcdef',2,3,'XYZ') AS StuffValue,
    TRANSLATE('abc','abc','xyz') AS TranslateValue,
    CONCAT('Nova',' ','SQL',NULL) AS ConcatValue,
    CONCAT_WS('|','Nova','SQL','2025') AS ConcatWsValue,
    QUOTENAME('odd name') AS QuoteNameValue,
    STRING_ESCAPE('"x"','json') AS EscapedJson;
GO

SELECT
    STRING_AGG(CONVERT(NVARCHAR(MAX),Code),N',')
    WITHIN GROUP(ORDER BY Code) AS Codes
FROM dbo.DmlTarget
WHERE Code IS NOT NULL;
GO

SELECT value,ordinal
FROM STRING_SPLIT('A,B,C',',',1);
GO

/* SQL Server 2025: optional SUBSTRING length argument */
SELECT SUBSTRING('abcdef',3) AS SubstringToEnd;
GO

/* SQL Server 2025: Unicode escape helper */
SELECT UNISTR(N'Nova \0044\0042') AS UnistrValue;
GO

/* ============================================================================
   150 - DATE/TIME FUNCTIONS
============================================================================ */

SELECT
    CURRENT_TIMESTAMP AS CurrentTimestamp,
    GETDATE() AS GetDate,
    GETUTCDATE() AS GetUtcDate,
    SYSDATETIME() AS SysDateTime,
    SYSUTCDATETIME() AS SysUtcDateTime,
    SYSDATETIMEOFFSET() AS SysDateTimeOffset,
    CURRENT_DATE AS CurrentDate2025,
    DATEFROMPARTS(2026,8,25) AS DateFromParts,
    DATETIMEFROMPARTS(2026,8,25,11,31,12,123) AS DateTimeFromParts,
    DATETIME2FROMPARTS(2026,8,25,11,31,12,1234567,7) AS DateTime2FromParts,
    DATETIMEOFFSETFROMPARTS(2026,8,25,11,31,12,1234567,7,0,7) AS DateTimeOffsetFromParts,
    TIMEFROMPARTS(11,31,12,1234567,7) AS TimeFromParts;
GO

SELECT
    DATEADD(DAY,10,CAST('2026-08-25' AS DATE)) AS DateAdd,
    DATEDIFF(DAY,'2026-01-01','2026-08-25') AS DateDiff,
    DATEDIFF_BIG(MILLISECOND,'2026-01-01','2026-08-25') AS DateDiffBig,
    DATEPART(ISO_WEEK,'2026-08-25') AS IsoWeek,
    DATENAME(MONTH,'2026-08-25') AS MonthName,
    DAY('2026-08-25') AS DayValue,
    MONTH('2026-08-25') AS MonthValue,
    YEAR('2026-08-25') AS YearValue,
    EOMONTH('2026-08-25') AS EndOfMonth,
    SWITCHOFFSET('2026-08-25T11:31:00+07:00','+00:00') AS SwitchedOffset,
    TODATETIMEOFFSET(CAST('2026-08-25T11:31:00' AS DATETIME2),'+07:00') AS ToOffset,
    CAST('2026-08-25T11:31:00' AS DATETIME2) AT TIME ZONE 'SE Asia Standard Time' AS AtTimeZone;
GO

/* SQL Server 2022+ */
SELECT
    DATETRUNC(MONTH,CAST('2026-08-25T11:31:12' AS DATETIME2)) AS DateTrunc,
    DATE_BUCKET(DAY,7,CAST('2026-08-25' AS DATE),CAST('2026-01-01' AS DATE)) AS DateBucket;
GO

/* SQL Server 2025: DATEADD number supports bigint */
SELECT DATEADD(MICROSECOND,CAST(100000 AS BIGINT),SYSUTCDATETIME()) AS BigintDateAdd;
GO

/* ============================================================================
   160 - MATH / CRYPTO / COMPRESSION / CHECKSUM
============================================================================ */

SELECT
    ABS(-10) AS AbsValue,
    ACOS(0.5) AS AcosValue,
    ASIN(0.5) AS AsinValue,
    ATAN(1.0) AS AtanValue,
    ATN2(1.0,1.0) AS Atn2Value,
    CEILING(12.3) AS CeilingValue,
    COS(1.0) AS CosValue,
    COT(1.0) AS CotValue,
    DEGREES(PI()) AS DegreesValue,
    EXP(1.0) AS ExpValue,
    FLOOR(12.9) AS FloorValue,
    LOG(10.0) AS LogValue,
    LOG10(100.0) AS Log10Value,
    PI() AS PiValue,
    POWER(2.0,10) AS PowerValue,
    RADIANS(180.0) AS RadiansValue,
    RAND(123) AS RandValue,
    ROUND(123.456,2) AS RoundValue,
    SIGN(-10) AS SignValue,
    SIN(1.0) AS SinValue,
    SQRT(144.0) AS SqrtValue,
    SQUARE(12.0) AS SquareValue,
    TAN(1.0) AS TanValue;
GO

SELECT
    HASHBYTES('SHA2_256',CONVERT(VARBINARY(MAX),'Nova')) AS Sha256,
    CHECKSUM('Nova',123) AS ChecksumValue,
    BINARY_CHECKSUM('Nova',123) AS BinaryChecksumValue,
    COMPRESS(N'Nova SQL Server 2025') AS CompressedValue;
GO

DECLARE @Compressed VARBINARY(MAX)=COMPRESS(N'Nova SQL');
SELECT CONVERT(NVARCHAR(MAX),DECOMPRESS(@Compressed)) AS DecompressedValue;
GO

/* SQL Server 2025 PRODUCT aggregate */
SELECT PRODUCT(CONVERT(DECIMAL(18,4),V.n)) AS ProductAggregate
FROM (VALUES(1),(2),(3),(4)) V(n);
GO

/* ============================================================================
   170 - GENERATE_SERIES / GREATEST / LEAST
============================================================================ */

SELECT value FROM GENERATE_SERIES(1,10,2);
GO

SELECT
    GREATEST(1,20,3,4) AS GreatestValue,
    LEAST(1,20,3,4) AS LeastValue;
GO

/* ============================================================================
   180 - JSON
============================================================================ */

DECLARE @Json NVARCHAR(MAX)=N'
{
  "id":1,
  "name":"Nova",
  "address":{"city":"Ha Noi","country":"VN"},
  "tags":["sql","server","2025"],
  "orders":[
    {"id":101,"amount":100.5},
    {"id":102,"amount":200.5}
  ]
}';

SELECT
    ISJSON(@Json) AS IsJson,
    JSON_VALUE(@Json,'$.name') AS JsonName,
    JSON_QUERY(@Json,'$.address') AS JsonAddress,
    JSON_PATH_EXISTS(@Json,'$.orders[0]') AS JsonPathExists;

SET @Json=JSON_MODIFY(@Json,'$.address.city','Da Nang');
SELECT @Json AS ModifiedJson;
GO

DECLARE @Json2 NVARCHAR(MAX)=N'
[
 {"id":1,"name":"A"},
 {"id":2,"name":"B"}
]';

SELECT *
FROM OPENJSON(@Json2)
WITH
(
    Id INT '$.id',
    Name NVARCHAR(100) '$.name'
);
GO

SELECT Id,Code,Qty
FROM dbo.DmlTarget
WHERE Code IS NOT NULL
FOR JSON PATH,ROOT('rows'),INCLUDE_NULL_VALUES;
GO

SELECT Id,Code
FROM dbo.DmlTarget
WHERE Id=(SELECT MIN(Id) FROM dbo.DmlTarget)
FOR JSON PATH,WITHOUT_ARRAY_WRAPPER;
GO

SELECT
    JSON_OBJECT('name':N'Nova','year':2025) AS JsonObjectValue,
    JSON_ARRAY(1,2,3,N'Nova') AS JsonArrayValue;
GO

SELECT
    JSON_ARRAYAGG(Code) AS JsonArrayAgg
FROM dbo.DmlTarget
WHERE Code IS NOT NULL;
GO

SELECT
    JSON_OBJECTAGG(Code:Qty) AS JsonObjectAgg
FROM dbo.DmlTarget
WHERE Code IS NOT NULL;
GO

/* SQL Server 2025 preview native JSON extras */
BEGIN TRY
    EXEC(N'
    DECLARE @J JSON = ''{"a":[1,2,3],"b":{"x":1}}'';
    SELECT JSON_QUERY(@J, ''$.a[*]'' WITH ARRAY WRAPPER) AS WildcardArray;
    SELECT JSON_CONTAINS(@J, 2, ''$.a'') AS Contains2;
    ');
END TRY
BEGIN CATCH
    SELECT ERROR_MESSAGE() AS Json2025PreviewError;
END CATCH;
GO

/* ============================================================================
   190 - XML / XQUERY
============================================================================ */

DECLARE @X XML=N'
<root>
  <item id="1"><name>A</name></item>
  <item id="2"><name>B</name></item>
</root>';

SELECT
    @X.exist('/root/item[@id="2"]') AS XmlExist,
    @X.value('(/root/item/@id)[1]','int') AS XmlValue,
    @X.query('/root/item') AS XmlQuery;

SELECT
    N.value('@id','int') AS ItemId,
    N.value('(name/text())[1]','nvarchar(100)') AS ItemName
FROM @X.nodes('/root/item') AS T(N);
GO

DECLARE @XM XML=N'<root><value>1</value></root>';
SET @XM.modify('replace value of (/root/value/text())[1] with "2"');
SELECT @XM AS ModifiedXml;
GO

/* legacy OPENXML */
DECLARE @DocHandle INT;
DECLARE @XmlText NVARCHAR(MAX)=N'<root><row id="1"/><row id="2"/></root>';
EXEC sys.sp_xml_preparedocument @DocHandle OUTPUT,@XmlText;
SELECT *
FROM OPENXML(@DocHandle,'/root/row',1)
WITH (id INT '@id');
EXEC sys.sp_xml_removedocument @DocHandle;
GO

/* XML indexes */
CREATE TABLE dbo.XmlIndexTest(Id INT PRIMARY KEY,Doc XML NOT NULL);
INSERT dbo.XmlIndexTest VALUES(1,N'<root><item id="1"/></root>');
CREATE PRIMARY XML INDEX PXML_XmlIndexTest ON dbo.XmlIndexTest(Doc);
CREATE XML INDEX PXML_XmlIndexTest_PATH ON dbo.XmlIndexTest(Doc)
USING XML INDEX PXML_XmlIndexTest FOR PATH;
CREATE XML INDEX PXML_XmlIndexTest_VALUE ON dbo.XmlIndexTest(Doc)
USING XML INDEX PXML_XmlIndexTest FOR VALUE;
CREATE XML INDEX PXML_XmlIndexTest_PROPERTY ON dbo.XmlIndexTest(Doc)
USING XML INDEX PXML_XmlIndexTest FOR PROPERTY;
GO

/* ============================================================================
   200 - SPATIAL
============================================================================ */

CREATE TABLE dbo.SpatialTest
(
    Id INT PRIMARY KEY,
    G GEOMETRY NULL,
    GG GEOGRAPHY NULL
);
INSERT dbo.SpatialTest
VALUES
(1,GEOMETRY::Point(0,0,0),GEOGRAPHY::Point(10,106,4326)),
(2,GEOMETRY::Point(3,4,0),GEOGRAPHY::Point(10.01,106.01,4326));

SELECT
    A.G.STDistance(B.G) AS GeometryDistance,
    A.GG.STDistance(B.GG) AS GeographyDistance
FROM dbo.SpatialTest A
JOIN dbo.SpatialTest B ON A.Id=1 AND B.Id=2;
GO

CREATE SPATIAL INDEX SIX_SpatialTest_G
ON dbo.SpatialTest(G)
USING GEOMETRY_GRID
WITH (BOUNDING_BOX=(-100,-100,100,100));
GO

/* ============================================================================
   210 - HIERARCHYID
============================================================================ */

CREATE TABLE dbo.HierarchyTest
(
    Node HIERARCHYID PRIMARY KEY,
    Name NVARCHAR(100)
);
INSERT dbo.HierarchyTest VALUES
(HIERARCHYID::GetRoot(),N'Root'),
(HIERARCHYID::Parse('/1/'),N'A'),
(HIERARCHYID::Parse('/1/1/'),N'A.1'),
(HIERARCHYID::Parse('/2/'),N'B');

SELECT
    Node.ToString() AS PathText,
    Node.GetLevel() AS NodeLevel,
    Name
FROM dbo.HierarchyTest
ORDER BY Node;

SELECT *
FROM dbo.HierarchyTest
WHERE Node.IsDescendantOf(HIERARCHYID::Parse('/1/'))=1;
GO

/* ============================================================================
   220 - TEMP OBJECTS / SELECT INTO
============================================================================ */

CREATE TABLE #LocalTemp(Id INT PRIMARY KEY,Value NVARCHAR(50));
INSERT #LocalTemp VALUES(1,N'A'),(2,N'B');
SELECT * FROM #LocalTemp;
DROP TABLE #LocalTemp;
GO

CREATE TABLE ##GlobalTemp(Id INT);
INSERT ##GlobalTemp VALUES(1);
SELECT * FROM ##GlobalTemp;
DROP TABLE ##GlobalTemp;
GO

DECLARE @TableVariable TABLE
(
    Id INT PRIMARY KEY,
    Value NVARCHAR(50)
);
INSERT @TableVariable VALUES(1,N'A'),(2,N'B');
SELECT * FROM @TableVariable;
GO

SELECT Id,Code,Qty
INTO #SelectIntoTest
FROM dbo.DmlTarget;
SELECT * FROM #SelectIntoTest;
DROP TABLE #SelectIntoTest;
GO

/* ============================================================================
   230 - VIEW / INDEXED VIEW / SYNONYM
============================================================================ */

CREATE OR ALTER VIEW dbo.vw_DmlTarget
AS
SELECT Id,Code,Qty,Price
FROM dbo.DmlTarget;
GO

SELECT * FROM dbo.vw_DmlTarget;
GO

CREATE SYNONYM dbo.DmlAlias FOR dbo.DmlTarget;
SELECT TOP(1) * FROM dbo.DmlAlias;
GO

/* indexed view */
CREATE TABLE dbo.IndexedViewBase
(
    Id INT NOT NULL PRIMARY KEY,
    Category INT NOT NULL,
    Amount DECIMAL(18,2) NOT NULL
);
INSERT dbo.IndexedViewBase VALUES(1,1,10),(2,1,20),(3,2,30);
GO

CREATE VIEW dbo.vw_IndexedAggregate
WITH SCHEMABINDING
AS
SELECT
    Category,
    COUNT_BIG(*) AS RowCount,
    SUM(Amount) AS TotalAmount
FROM dbo.IndexedViewBase
GROUP BY Category;
GO

CREATE UNIQUE CLUSTERED INDEX CIX_vw_IndexedAggregate
ON dbo.vw_IndexedAggregate(Category);
GO

SELECT * FROM dbo.vw_IndexedAggregate WITH (NOEXPAND);
GO

/* ============================================================================
   240 - PROCEDURES / FUNCTIONS / EXECUTE / RETURN / OUTPUT PARAMS
============================================================================ */

CREATE OR ALTER PROCEDURE dbo.usp_ProcTest
    @Input INT,
    @Output INT OUTPUT
AS
BEGIN
    SET NOCOUNT ON;
    SET @Output=@Input*2;
    RETURN 7;
END;
GO

DECLARE @Out INT,@ReturnCode INT;
EXEC @ReturnCode=dbo.usp_ProcTest
    @Input=21,
    @Output=@Out OUTPUT;
SELECT @Out AS OutputValue,@ReturnCode AS ReturnCode;
GO

CREATE OR ALTER FUNCTION dbo.fn_ScalarTest(@x INT)
RETURNS INT
WITH SCHEMABINDING
AS
BEGIN
    RETURN @x*@x;
END;
GO

SELECT dbo.fn_ScalarTest(12) AS ScalarFunctionResult;
GO

CREATE OR ALTER FUNCTION dbo.fn_InlineTvf(@MinQty INT)
RETURNS TABLE
AS
RETURN
(
    SELECT Id,Code,Qty
    FROM dbo.DmlTarget
    WHERE Qty>=@MinQty
);
GO

SELECT * FROM dbo.fn_InlineTvf(1);
GO

CREATE OR ALTER FUNCTION dbo.fn_MultiTvf(@N INT)
RETURNS @T TABLE(n INT PRIMARY KEY)
AS
BEGIN
    DECLARE @i INT=1;
    WHILE @i<=@N
    BEGIN
        INSERT @T VALUES(@i);
        SET @i+=1;
    END;
    RETURN;
END;
GO

SELECT * FROM dbo.fn_MultiTvf(5);
GO

/* table-valued parameter */
CREATE OR ALTER PROCEDURE dbo.usp_TvpTest
    @Ids dbo.IntIdList READONLY
AS
BEGIN
    SELECT D.*
    FROM dbo.DmlTarget D
    JOIN @Ids I ON I.Id=D.Id;
END;
GO

DECLARE @Ids dbo.IntIdList;
INSERT @Ids VALUES(1),(2);
EXEC dbo.usp_TvpTest @Ids=@Ids;
GO

/* ============================================================================
   250 - DYNAMIC SQL
============================================================================ */

DECLARE @Sql NVARCHAR(MAX)=N'
SELECT @RowCount=COUNT(*)
FROM dbo.DmlTarget
WHERE Qty>=@MinQty;';

DECLARE @Rows INT;

EXEC sys.sp_executesql
    @Sql,
    N'@MinQty INT,@RowCount INT OUTPUT',
    @MinQty=1,
    @RowCount=@Rows OUTPUT;

SELECT @Rows AS DynamicSqlRows;
GO

/* ============================================================================
   260 - CONTROL FLOW / LABEL / GOTO / BREAK / CONTINUE / WAITFOR
============================================================================ */

DECLARE @n INT=0;

WHILE @n<5
BEGIN
    SET @n+=1;
    IF @n=2 CONTINUE;
    IF @n=4 BREAK;
END;

IF @n=4
    PRINT N'IF/WHILE/BREAK/CONTINUE PASS';
ELSE
    PRINT N'Unexpected';
GO

GOTO JumpTarget;
SELECT 'should not execute' AS X;
JumpTarget:
SELECT 'GOTO PASS' AS Result;
GO

/* WAITFOR intentionally tiny */
WAITFOR DELAY '00:00:00.010';
SELECT 'WAITFOR PASS' AS Result;
GO

/* ============================================================================
   270 - ERROR HANDLING: TRY/CATCH / THROW / RAISERROR
============================================================================ */

BEGIN TRY
    THROW 51000,'Throw test',1;
END TRY
BEGIN CATCH
    SELECT
        ERROR_NUMBER() AS ErrorNumber,
        ERROR_SEVERITY() AS ErrorSeverity,
        ERROR_STATE() AS ErrorState,
        ERROR_PROCEDURE() AS ErrorProcedure,
        ERROR_LINE() AS ErrorLine,
        ERROR_MESSAGE() AS ErrorMessage;
END CATCH;
GO

BEGIN TRY
    RAISERROR('RAISERROR compatibility test',16,1);
END TRY
BEGIN CATCH
    SELECT ERROR_MESSAGE() AS RaiserrorCaught;
END CATCH;
GO

/* ============================================================================
   280 - TRANSACTIONS / SAVEPOINT / NESTED TRANCOUNT / XACT_STATE
============================================================================ */

CREATE TABLE dbo.TxTest(Id INT PRIMARY KEY,Value INT);
INSERT dbo.TxTest VALUES(1,0);
GO

BEGIN TRANSACTION TxOuter;
UPDATE dbo.TxTest SET Value=1 WHERE Id=1;

SAVE TRANSACTION SaveA;
UPDATE dbo.TxTest SET Value=2 WHERE Id=1;
ROLLBACK TRANSACTION SaveA;

BEGIN TRANSACTION TxInner;
UPDATE dbo.TxTest SET Value=3 WHERE Id=1;
COMMIT TRANSACTION TxInner;

SELECT @@TRANCOUNT AS TranCountBeforeOuterCommit,XACT_STATE() AS XactState;
COMMIT TRANSACTION TxOuter;

SELECT * FROM dbo.TxTest;
GO

SET XACT_ABORT ON;
BEGIN TRY
    BEGIN TRANSACTION;
    UPDATE dbo.TxTest SET Value=4 WHERE Id=1;
    COMMIT;
END TRY
BEGIN CATCH
    IF XACT_STATE()<>0 ROLLBACK;
    THROW;
END CATCH;
SET XACT_ABORT OFF;
GO

/* ============================================================================
   290 - CURSORS
============================================================================ */

DECLARE @Id INT;

DECLARE C CURSOR LOCAL FAST_FORWARD
FOR
SELECT TOP(3) Id FROM dbo.DmlTarget ORDER BY Id;

OPEN C;

FETCH NEXT FROM C INTO @Id;

WHILE @@FETCH_STATUS=0
BEGIN
    SELECT @Id AS CursorId;
    FETCH NEXT FROM C INTO @Id;
END;

CLOSE C;
DEALLOCATE C;
GO

/* ============================================================================
   300 - DML TRIGGER / DDL TRIGGER
============================================================================ */

CREATE TABLE dbo.TriggerAudit
(
    AuditId BIGINT IDENTITY PRIMARY KEY,
    ActionName VARCHAR(10),
    RowId INT,
    OldQty INT NULL,
    NewQty INT NULL,
    AtTime DATETIME2 DEFAULT SYSDATETIME()
);
GO

CREATE OR ALTER TRIGGER dbo.tr_DmlTarget_Audit
ON dbo.DmlTarget
AFTER INSERT,UPDATE,DELETE
AS
BEGIN
    SET NOCOUNT ON;

    INSERT dbo.TriggerAudit(ActionName,RowId,OldQty,NewQty)
    SELECT
        CASE
            WHEN I.Id IS NOT NULL AND D.Id IS NULL THEN 'INSERT'
            WHEN I.Id IS NOT NULL AND D.Id IS NOT NULL THEN 'UPDATE'
            ELSE 'DELETE'
        END,
        COALESCE(I.Id,D.Id),
        D.Qty,
        I.Qty
    FROM inserted I
    FULL JOIN deleted D ON D.Id=I.Id;
END;
GO

UPDATE dbo.DmlTarget SET Qty=Qty+1 WHERE Code IN('A','B');
SELECT * FROM dbo.TriggerAudit;
GO

CREATE TABLE dbo.DdlAudit
(
    Id INT IDENTITY PRIMARY KEY,
    EventType SYSNAME,
    ObjectName SYSNAME,
    EventData XML,
    AtTime DATETIME2 DEFAULT SYSDATETIME()
);
GO

CREATE OR ALTER TRIGGER tr_Nova_DDL
ON DATABASE
FOR CREATE_TABLE,ALTER_TABLE,DROP_TABLE
AS
BEGIN
    DECLARE @E XML=EVENTDATA();

    INSERT dbo.DdlAudit(EventType,ObjectName,EventData)
    VALUES
    (
        @E.value('(/EVENT_INSTANCE/EventType)[1]','sysname'),
        @E.value('(/EVENT_INSTANCE/ObjectName)[1]','sysname'),
        @E
    );
END;
GO

CREATE TABLE dbo.DdlTriggerProbe(Id INT);
ALTER TABLE dbo.DdlTriggerProbe ADD X INT;
DROP TABLE dbo.DdlTriggerProbe;
SELECT * FROM dbo.DdlAudit;
GO

/* ============================================================================
   310 - INDEXES / STATISTICS / HINTS
============================================================================ */

CREATE INDEX IX_DmlTarget_Qty_Price
ON dbo.DmlTarget(Qty DESC,Price ASC)
INCLUDE(Code,Note);
GO

CREATE INDEX IX_DmlTarget_Filtered
ON dbo.DmlTarget(Code)
WHERE Code IS NOT NULL;
GO

CREATE STATISTICS ST_DmlTarget_Qty_Price
ON dbo.DmlTarget(Qty,Price)
WITH FULLSCAN;
GO

UPDATE STATISTICS dbo.DmlTarget WITH FULLSCAN;
GO

SELECT *
FROM dbo.DmlTarget WITH (INDEX(IX_DmlTarget_Qty_Price))
WHERE Qty>0
OPTION(RECOMPILE,MAXDOP 1);
GO

SELECT *
FROM dbo.DmlTarget WITH (NOLOCK)
WHERE Id>0;
GO

SELECT *
FROM dbo.DmlTarget WITH (UPDLOCK,HOLDLOCK,ROWLOCK)
WHERE Id=(SELECT MIN(Id) FROM dbo.DmlTarget);
GO

SELECT *
FROM dbo.DmlTarget
WHERE Qty>0
OPTION
(
    FORCE ORDER,
    LOOP JOIN,
    FAST 10,
    MAXRECURSION 100
);
GO

/* ============================================================================
   320 - COLUMNSTORE
============================================================================ */

CREATE TABLE dbo.ColumnstoreFact
(
    Id BIGINT NOT NULL,
    GroupId INT NOT NULL,
    Qty INT NOT NULL,
    Amount DECIMAL(18,2) NOT NULL,
    D DATE NOT NULL
);
GO

INSERT dbo.ColumnstoreFact
SELECT
    V.value,
    V.value%20,
    (V.value%10)+1,
    CONVERT(DECIMAL(18,2),(V.value%1000)*1.25),
    DATEADD(DAY,V.value%365,CONVERT(DATE,'2026-01-01'))
FROM GENERATE_SERIES(1,5000) V;
GO

CREATE CLUSTERED COLUMNSTORE INDEX CCI_ColumnstoreFact
ON dbo.ColumnstoreFact;
GO

SELECT GroupId,SUM(Qty*Amount) AS Revenue,COUNT_BIG(*) AS RowsCount
FROM dbo.ColumnstoreFact
GROUP BY GroupId;
GO

/* ============================================================================
   330 - PARTITIONING
============================================================================ */

CREATE PARTITION FUNCTION pf_NovaDate(DATE)
AS RANGE RIGHT FOR VALUES
(
    '2025-01-01',
    '2026-01-01',
    '2027-01-01'
);
GO

CREATE PARTITION SCHEME ps_NovaDate
AS PARTITION pf_NovaDate
ALL TO ([PRIMARY]);
GO

CREATE TABLE dbo.PartitionedTable
(
    Id BIGINT NOT NULL,
    D DATE NOT NULL,
    Amount DECIMAL(18,2),
    CONSTRAINT PK_PartitionedTable PRIMARY KEY CLUSTERED(Id,D)
)
ON ps_NovaDate(D);
GO

INSERT dbo.PartitionedTable VALUES
(1,'2024-01-01',10),
(2,'2025-01-01',20),
(3,'2026-01-01',30),
(4,'2027-01-01',40),
(5,'2028-01-01',50);
GO

SELECT
    $PARTITION.pf_NovaDate(D) AS PartitionNo,
    COUNT(*) AS Cnt
FROM dbo.PartitionedTable
GROUP BY $PARTITION.pf_NovaDate(D)
ORDER BY PartitionNo;
GO

/* ============================================================================
   340 - TEMPORAL SYSTEM-VERSIONED TABLE
============================================================================ */

CREATE TABLE dbo.TemporalEmployee
(
    EmployeeId INT IDENTITY PRIMARY KEY,
    Name NVARCHAR(100) NOT NULL,
    Salary DECIMAL(18,2) NOT NULL,

    SysStart DATETIME2 GENERATED ALWAYS AS ROW START HIDDEN NOT NULL
        CONSTRAINT DF_TemporalEmployee_Start DEFAULT SYSUTCDATETIME(),

    SysEnd DATETIME2 GENERATED ALWAYS AS ROW END HIDDEN NOT NULL
        CONSTRAINT DF_TemporalEmployee_End
        DEFAULT CONVERT(DATETIME2,'9999-12-31 23:59:59.9999999'),

    PERIOD FOR SYSTEM_TIME(SysStart,SysEnd)
)
WITH
(
    SYSTEM_VERSIONING=ON
    (
        HISTORY_TABLE=dbo.TemporalEmployeeHistory,
        DATA_CONSISTENCY_CHECK=ON
    )
);
GO

INSERT dbo.TemporalEmployee(Name,Salary) VALUES(N'Nova',100);
UPDATE dbo.TemporalEmployee SET Salary=200 WHERE EmployeeId=1;

SELECT * FROM dbo.TemporalEmployee FOR SYSTEM_TIME ALL;
SELECT * FROM dbo.TemporalEmployee
FOR SYSTEM_TIME AS OF SYSUTCDATETIME();
GO

/* ============================================================================
   350 - GRAPH NODE / EDGE / MATCH
============================================================================ */

CREATE TABLE dbo.PersonNode
(
    PersonId INT,
    Name NVARCHAR(100)
) AS NODE;
GO

CREATE TABLE dbo.KnowsEdge
(
    SinceYear INT
) AS EDGE;
GO

INSERT dbo.PersonNode(PersonId,Name)
VALUES(1,N'A'),(2,N'B'),(3,N'C');
GO

INSERT dbo.KnowsEdge($from_id,$to_id,SinceYear)
SELECT A.$node_id,B.$node_id,2025
FROM dbo.PersonNode A
CROSS JOIN dbo.PersonNode B
WHERE A.PersonId=1 AND B.PersonId=2;
GO

SELECT A.Name AS FromName,E.SinceYear,B.Name AS ToName
FROM dbo.PersonNode A,dbo.KnowsEdge E,dbo.PersonNode B
WHERE MATCH(A-(E)->B);
GO

/* ============================================================================
   360 - LEDGER (SQL Server 2022+)
============================================================================ */

/* Append-only ledger */
CREATE TABLE dbo.LedgerEvent
(
    EventId BIGINT IDENTITY PRIMARY KEY,
    EventName NVARCHAR(100) NOT NULL,
    CreatedAt DATETIME2 NOT NULL DEFAULT SYSUTCDATETIME()
)
WITH
(
    LEDGER=ON,
    APPEND_ONLY=ON
);
GO

INSERT dbo.LedgerEvent(EventName) VALUES(N'created');
SELECT * FROM dbo.LedgerEvent;
GO

/* ============================================================================
   370 - SECURITY: USERS / ROLES / GRANT / DENY / REVOKE / EXECUTE AS
============================================================================ */

CREATE USER NovaTestUser WITHOUT LOGIN;
CREATE ROLE NovaTestRole;
ALTER ROLE NovaTestRole ADD MEMBER NovaTestUser;
GO

GRANT SELECT ON dbo.DmlTarget TO NovaTestRole;
DENY DELETE ON dbo.DmlTarget TO NovaTestRole;
REVOKE DELETE ON dbo.DmlTarget TO NovaTestRole;
GO

EXECUTE AS USER='NovaTestUser';
SELECT TOP(1) * FROM dbo.DmlTarget;
REVERT;
GO

/* ============================================================================
   380 - DYNAMIC DATA MASKING
============================================================================ */

CREATE TABLE dbo.MaskingTest
(
    Id INT PRIMARY KEY,
    Email VARCHAR(200) MASKED WITH (FUNCTION='email()'),
    Phone VARCHAR(30) MASKED WITH (FUNCTION='partial(2,"XXXX",2)'),
    Secret VARCHAR(100) MASKED WITH (FUNCTION='default()')
);
INSERT dbo.MaskingTest VALUES
(1,'nova@example.com','0912345678','secret');
GO

/* ============================================================================
   390 - ROW LEVEL SECURITY / SESSION_CONTEXT
============================================================================ */

CREATE SCHEMA Security;
GO

CREATE TABLE dbo.TenantData
(
    Id INT IDENTITY PRIMARY KEY,
    TenantId INT NOT NULL,
    Value NVARCHAR(100)
);
INSERT dbo.TenantData(TenantId,Value)
VALUES(1,N'T1-A'),(1,N'T1-B'),(2,N'T2-A');
GO

CREATE FUNCTION Security.fn_TenantPredicate(@TenantId INT)
RETURNS TABLE
WITH SCHEMABINDING
AS
RETURN
(
    SELECT 1 AS Allowed
    WHERE @TenantId=TRY_CONVERT(INT,SESSION_CONTEXT(N'TenantId'))
);
GO

CREATE SECURITY POLICY Security.TenantPolicy
ADD FILTER PREDICATE Security.fn_TenantPredicate(TenantId)
ON dbo.TenantData
WITH(STATE=ON);
GO

EXEC sys.sp_set_session_context @key=N'TenantId',@value=1;
SELECT * FROM dbo.TenantData;
GO

/* ============================================================================
   400 - KEYS / CERTIFICATES / SYMMETRIC ENCRYPTION
============================================================================ */

CREATE MASTER KEY ENCRYPTION BY PASSWORD='Nova-Conformance-Only-Strong-Pass-2026!';
GO

CREATE CERTIFICATE NovaCert
WITH SUBJECT='Nova conformance certificate';
GO

CREATE SYMMETRIC KEY NovaSymmetricKey
WITH ALGORITHM=AES_256
ENCRYPTION BY CERTIFICATE NovaCert;
GO

OPEN SYMMETRIC KEY NovaSymmetricKey
DECRYPTION BY CERTIFICATE NovaCert;

DECLARE @Cipher VARBINARY(MAX)=EncryptByKey(Key_GUID('NovaSymmetricKey'),N'secret');
SELECT
    @Cipher AS CipherText,
    CONVERT(NVARCHAR(100),DecryptByKey(@Cipher)) AS PlainText;

CLOSE SYMMETRIC KEY NovaSymmetricKey;
GO

/* ============================================================================
   410 - SERVICE BROKER LANGUAGE SURFACE
============================================================================ */

CREATE MESSAGE TYPE [//Nova/Request]
VALIDATION=WELL_FORMED_XML;
GO

CREATE MESSAGE TYPE [//Nova/Reply]
VALIDATION=WELL_FORMED_XML;
GO

CREATE CONTRACT [//Nova/Contract]
(
    [//Nova/Request] SENT BY INITIATOR,
    [//Nova/Reply] SENT BY TARGET
);
GO

CREATE QUEUE dbo.NovaQueue;
GO

CREATE SERVICE [//Nova/Service]
ON QUEUE dbo.NovaQueue
([//Nova/Contract]);
GO

/* BEGIN DIALOG / SEND / RECEIVE */
DECLARE @Dialog UNIQUEIDENTIFIER;

BEGIN DIALOG CONVERSATION @Dialog
    FROM SERVICE [//Nova/Service]
    TO SERVICE N'//Nova/Service'
    ON CONTRACT [//Nova/Contract]
    WITH ENCRYPTION=OFF;

SEND ON CONVERSATION @Dialog
MESSAGE TYPE [//Nova/Request]
(N'<request>hello</request>');

WAITFOR
(
    RECEIVE TOP(1)
        conversation_handle,
        message_type_name,
        message_body
    FROM dbo.NovaQueue
),TIMEOUT 1000;

END CONVERSATION @Dialog;
GO

/* ============================================================================
   420 - METADATA / CATALOG / INFORMATION_SCHEMA / DMVs
============================================================================ */

SELECT * FROM INFORMATION_SCHEMA.TABLES;
SELECT * FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_NAME='DmlTarget';
GO

SELECT
    S.name AS SchemaName,
    T.name AS TableName,
    C.name AS ColumnName,
    TY.name AS TypeName,
    C.max_length,
    C.precision,
    C.scale,
    C.is_nullable,
    C.is_identity,
    C.is_computed
FROM sys.tables T
JOIN sys.schemas S ON S.schema_id=T.schema_id
JOIN sys.columns C ON C.object_id=T.object_id
JOIN sys.types TY ON TY.user_type_id=C.user_type_id
ORDER BY S.name,T.name,C.column_id;
GO

SELECT * FROM sys.indexes WHERE object_id=OBJECT_ID('dbo.DmlTarget');
SELECT * FROM sys.foreign_keys;
SELECT * FROM sys.check_constraints;
SELECT * FROM sys.default_constraints;
SELECT * FROM sys.sql_modules;
SELECT * FROM sys.sequences;
SELECT * FROM sys.partition_functions;
SELECT * FROM sys.partition_schemes;
GO

SELECT * FROM sys.dm_exec_sessions WHERE session_id=@@SPID;
SELECT * FROM sys.dm_exec_connections WHERE session_id=@@SPID;
GO

/* ============================================================================
   430 - DBCC
============================================================================ */

DBCC CHECKDB (NovaConformance2025) WITH NO_INFOMSGS;
GO

DBCC CHECKTABLE ('dbo.DmlTarget') WITH NO_INFOMSGS;
GO

DBCC SHOW_STATISTICS ('dbo.DmlTarget','IX_DmlTarget_Qty_Price');
GO

/* ============================================================================
   490 - DATABASE SCOPED CONFIG / QUERY STORE
============================================================================ */

ALTER DATABASE NovaConformance2025 SET QUERY_STORE=ON;
GO

ALTER DATABASE SCOPED CONFIGURATION SET MAXDOP=1;
ALTER DATABASE SCOPED CONFIGURATION SET LEGACY_CARDINALITY_ESTIMATION=OFF;
GO

SELECT * FROM sys.database_scoped_configurations;
GO

/* ============================================================================
   500 - SQL SERVER 2025 REGULAR EXPRESSIONS
============================================================================ */

SELECT
    REGEXP_LIKE('nova-2025','^nova-[0-9]{4}$') AS RegexLike,
    REGEXP_COUNT('a1b2c3','[0-9]') AS RegexCount,
    REGEXP_INSTR('abc123','[0-9]+') AS RegexInstr,
    REGEXP_SUBSTR('abc123xyz','[0-9]+') AS RegexSubstr,
    REGEXP_REPLACE('abc123xyz','[0-9]+','###') AS RegexReplace;
GO

SELECT *
FROM REGEXP_MATCHES('abc-123 xyz-456','([a-z]+)-([0-9]+)');
GO

SELECT *
FROM REGEXP_SPLIT_TO_TABLE('a,b;;c','[,;]+');
GO

/* ============================================================================
   510 - SQL SERVER 2025 FUZZY STRING [PREVIEW_FEATURES may be required]
============================================================================ */

BEGIN TRY
    ALTER DATABASE SCOPED CONFIGURATION SET PREVIEW_FEATURES=ON;
END TRY
BEGIN CATCH
    SELECT ERROR_MESSAGE() AS PreviewFeaturesEnableError;
END CATCH;
GO

BEGIN TRY
    EXEC(N'
    SELECT
        EDIT_DISTANCE(''kitten'',''sitting'') AS EditDistance,
        EDIT_DISTANCE_SIMILARITY(''nova'',''novadb'') AS EditSimilarity,
        JARO_WINKLER_DISTANCE(''martha'',''marhta'') AS JaroDistance,
        JARO_WINKLER_SIMILARITY(''martha'',''marhta'') AS JaroSimilarity;
    ');
END TRY
BEGIN CATCH
    SELECT ERROR_MESSAGE() AS FuzzyStringPreviewError;
END CATCH;
GO

/* ============================================================================
   520 - SQL SERVER 2025 VECTOR FUNCTIONS / VECTOR INDEX [some preview-gated]
============================================================================ */

BEGIN TRY
    EXEC(N'
    CREATE TABLE dbo.VectorSearchTest
    (
        Id INT PRIMARY KEY,
        V VECTOR(3)
    );

    INSERT dbo.VectorSearchTest VALUES
    (1,''[1,0,0]''),
    (2,''[0,1,0]''),
    (3,''[0,0,1]'');

    SELECT
        Id,
        VECTOR_DISTANCE(''cosine'',V,CAST(''[1,0,0]'' AS VECTOR(3))) AS Distance
    FROM dbo.VectorSearchTest
    ORDER BY Distance;
    ');
END TRY
BEGIN CATCH
    SELECT ERROR_MESSAGE() AS VectorFunctionError;
END CATCH;
GO

/* CREATE VECTOR INDEX / VECTOR_SEARCH are kept dynamic because availability
   can depend on build / preview state. */
BEGIN TRY
    EXEC(N'
    CREATE VECTOR INDEX IX_VectorSearchTest_V
    ON dbo.VectorSearchTest(V)
    WITH (METRIC = ''cosine'', TYPE = ''diskann'');
    ');
END TRY
BEGIN CATCH
    SELECT ERROR_MESSAGE() AS VectorIndexPreviewError;
END CATCH;
GO

/* ============================================================================
   590 - LEGACY / DEPRECATED LANGUAGE SURFACE
============================================================================ */

/* Old CREATE DEFAULT / CREATE RULE still exist for compatibility but are
   deprecated. Kept in isolated names. */
CREATE DEFAULT dbo.NovaLegacyDefault AS 0;
GO

CREATE RULE dbo.NovaPositiveRule AS @value>=0;
GO

CREATE TABLE dbo.LegacyBindingTest
(
    Id INT,
    Value INT
);
GO

EXEC sys.sp_bindefault 'dbo.NovaLegacyDefault','dbo.LegacyBindingTest.Value';
EXEC sys.sp_bindrule 'dbo.NovaPositiveRule','dbo.LegacyBindingTest.Value';
GO

INSERT dbo.LegacyBindingTest(Id) VALUES(1);
SELECT * FROM dbo.LegacyBindingTest;
GO

/* ============================================================================
   600 - MONSTER COMPOUND QUERY
============================================================================ */

WITH
Base AS
(
    SELECT
        Id,Code,Qty,Price,
        COALESCE(Code,'(null)') AS SafeCode,
        Qty*Price AS Extended
    FROM dbo.DmlTarget
    WHERE Qty IS NOT NULL
),
Stats AS
(
    SELECT
        COUNT_BIG(*) AS TotalRows,
        AVG(CONVERT(DECIMAL(38,6),Qty)) AS AvgQty,
        SUM(CONVERT(DECIMAL(38,6),Qty*Price)) AS TotalExtended
    FROM Base
),
Ranked AS
(
    SELECT
        B.*,
        S.TotalRows,
        S.AvgQty,
        S.TotalExtended,

        ROW_NUMBER() OVER(ORDER BY B.Qty DESC,B.Id) AS rn,
        DENSE_RANK() OVER(ORDER BY B.Qty DESC) AS dr,
        LAG(B.Qty,1,0) OVER(ORDER BY B.Id) AS PrevQty,
        LEAD(B.Qty,1,0) OVER(ORDER BY B.Id) AS NextQty,
        SUM(B.Qty) OVER
        (
            ORDER BY B.Id
            ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
        ) AS RunningQty,

        PERCENTILE_CONT(0.5)
        WITHIN GROUP(ORDER BY B.Qty)
        OVER() AS MedianQty

    FROM Base B
    CROSS JOIN Stats S
)
SELECT TOP(100)
    R.Id,
    R.Code,
    R.Qty,
    R.Price,
    R.Extended,
    R.AvgQty,
    R.MedianQty,
    R.rn,
    R.dr,
    R.PrevQty,
    R.NextQty,
    R.RunningQty,

    C.Classification,
    P.PriceWithTax,

    (
        SELECT COUNT(*)
        FROM dbo.DmlTarget X
        WHERE X.Qty>R.Qty
    ) AS MoreQtyRows,

    CASE
        WHEN EXISTS
        (
            SELECT 1 FROM dbo.DmlTarget X
            WHERE X.Id=R.Id AND X.Price>0
        )
        THEN CAST(1 AS BIT)
        ELSE CAST(0 AS BIT)
    END AS ExistsPositivePrice,

    JSON_OBJECT
    (
        'id':R.Id,
        'code':R.Code,
        'qty':R.Qty
    ) AS JsonProjection

FROM Ranked R

CROSS APPLY
(
    SELECT
        CASE
            WHEN R.Qty>=R.AvgQty*2 THEN 'VERY_HIGH'
            WHEN R.Qty>R.AvgQty THEN 'HIGH'
            WHEN R.Qty=R.AvgQty THEN 'AVERAGE'
            ELSE 'LOW'
        END AS Classification
) C

CROSS APPLY
(
    SELECT R.Price*1.10 AS PriceWithTax
) P

WHERE
    R.Id IN
    (
        SELECT Id
        FROM dbo.DmlTarget
        WHERE Price>=0
    )

ORDER BY
    R.Qty DESC,
    R.Id

OPTION
(
    RECOMPILE,
    MAXDOP 1
);
GO

/* ============================================================================
   610 - FINAL SURFACE INFO
============================================================================ */

SELECT
    @@VERSION AS VersionText,
    SERVERPROPERTY('ProductVersion') AS ProductVersion,
    SERVERPROPERTY('ProductLevel') AS ProductLevel,
    SERVERPROPERTY('Edition') AS Edition,
    DATABASEPROPERTYEX(DB_NAME(),'CompatibilityLevel') AS CompatibilityLevel,
    DB_NAME() AS DatabaseName,
    @@SPID AS SessionId,
    @@TRANCOUNT AS TranCount,
    XACT_STATE() AS XactState;
GO

SELECT N'CORE SUITE REACHED END' AS Result,SYSDATETIMEOFFSET() AS FinishedAt;
GO
"""

re_go = re.compile(r'(?im)^\s*GO\s*;?\s*$')
batches = re_go.split(conformance_script)

url = "http://127.0.0.1:8787/v1/admin/databases/test_db/execute"

print(f"Total batches to test: {len(batches)}")
for i, batch in enumerate(batches):
    trimmed = batch.strip()
    if not trimmed or (trimmed.startswith('--') and '\n' not in trimmed):
        continue
    
    resp = requests.post(url, json={"sql": batch})
    if resp.status_code != 200:
        print(f"Batch {i+1} FAILED ({resp.status_code}):\n{resp.text}\nSQL:\n{batch}\n")
        break
    else:
        print(f"Batch {i+1} passed!")
else:
    print("All batches in 2025 Conformance Suite passed!")
