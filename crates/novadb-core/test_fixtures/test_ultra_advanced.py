import requests, re

ultra_script = """/* =====================================================================
   NOVADB vs SQL SERVER
   ULTRA-ADVANCED T-SQL STRUCTURE TEST
   Level: Advanced -> Enterprise -> SQL Server Specific

   Một số phần yêu cầu SQL Server 2016/2017/2019/2022.
   ===================================================================== */

USE NovaSqlServerLab;
GO


/* =====================================================================
   01. ROWVERSION + COMPUTED PERSISTED + FILTERED UNIQUE INDEX
   ===================================================================== */

DROP TABLE IF EXISTS dbo.AdvancedAccount;
GO

CREATE TABLE dbo.AdvancedAccount
(
    AccountID BIGINT IDENTITY(1,1)
        NOT NULL
        PRIMARY KEY,

    FirstName NVARCHAR(100)
        NOT NULL,

    LastName NVARCHAR(100)
        NOT NULL,

    Email VARCHAR(250),

    Balance DECIMAL(19,4)
        NOT NULL
        DEFAULT 0,

    FullName AS
    (
        LTRIM(RTRIM(FirstName))
        + N' '
        + LTRIM(RTRIM(LastName))
    )
    PERSISTED,

    VersionStamp ROWVERSION
);
GO


CREATE UNIQUE INDEX UX_AdvancedAccount_Email

ON dbo.AdvancedAccount(Email)

WHERE Email IS NOT NULL;
GO


CREATE INDEX IX_AdvancedAccount_FullName

ON dbo.AdvancedAccount(FullName)

INCLUDE
(
    Email,
    Balance
);
GO


INSERT dbo.AdvancedAccount
(
    FirstName,
    LastName,
    Email,
    Balance
)
VALUES
(
    N'Nguyễn',
    N'Văn An',
    'an@nova.local',
    1000000
),
(
    N'Trần',
    N'Minh Tuấn',
    'tuan@nova.local',
    2000000
);
GO


SELECT *
FROM dbo.AdvancedAccount;
GO


/* =====================================================================
   02. TEMPORAL TABLE / SYSTEM VERSIONED TABLE

   SQL Server 2016+
   Đây là cấu trúc rất quan trọng để test compatibility.
   ===================================================================== */

IF OBJECT_ID(N'dbo.AdvEmployeeTemporal', N'U') IS NOT NULL
BEGIN

    IF EXISTS
    (
        SELECT 1
        FROM sys.tables
        WHERE object_id =
              OBJECT_ID(N'dbo.AdvEmployeeTemporal')
          AND temporal_type = 2
    )
    BEGIN

        ALTER TABLE dbo.AdvEmployeeTemporal
        SET
        (
            SYSTEM_VERSIONING = OFF
        );

    END;

    DROP TABLE dbo.AdvEmployeeTemporal;

END;
GO


DROP TABLE IF EXISTS dbo.AdvEmployeeTemporalHistory;
GO


CREATE TABLE dbo.AdvEmployeeTemporal
(
    EmployeeID INT IDENTITY(1,1)
        PRIMARY KEY,

    FullName NVARCHAR(200)
        NOT NULL,

    Salary DECIMAL(18,2)
        NOT NULL,

    Department NVARCHAR(100),

    ValidFrom DATETIME2(7)
        GENERATED ALWAYS AS ROW START
        HIDDEN
        NOT NULL

        CONSTRAINT DF_AdvTemporal_From
        DEFAULT SYSUTCDATETIME(),

    ValidTo DATETIME2(7)
        GENERATED ALWAYS AS ROW END
        HIDDEN
        NOT NULL

        CONSTRAINT DF_AdvTemporal_To
        DEFAULT
        CONVERT
        (
            DATETIME2(7),
            '9999-12-31 23:59:59.9999999'
        ),

    PERIOD FOR SYSTEM_TIME
    (
        ValidFrom,
        ValidTo
    )
)

WITH
(
    SYSTEM_VERSIONING = ON
    (
        HISTORY_TABLE =
            dbo.AdvEmployeeTemporalHistory,

        DATA_CONSISTENCY_CHECK = ON
    )
);
GO


INSERT dbo.AdvEmployeeTemporal
(
    FullName,
    Salary,
    Department
)
VALUES
(
    N'Nguyễn Văn A',
    15000000,
    N'IT'
);
GO


UPDATE dbo.AdvEmployeeTemporal

SET Salary = 20000000

WHERE EmployeeID = 1;
GO


SELECT *
FROM dbo.AdvEmployeeTemporal;
GO


/* Toàn bộ lịch sử */

SELECT *
FROM dbo.AdvEmployeeTemporal

FOR SYSTEM_TIME ALL

ORDER BY
    EmployeeID,
    ValidFrom;
GO


/* Query dữ liệu tại một thời điểm */

DECLARE @TemporalTime DATETIME2 =
    SYSUTCDATETIME();

SELECT *
FROM dbo.AdvEmployeeTemporal

FOR SYSTEM_TIME AS OF @TemporalTime;
GO


/* =====================================================================
   03. SQL SERVER GRAPH - NODE / EDGE / MATCH

   SQL Server 2017+
   ===================================================================== */

DROP TABLE IF EXISTS dbo.AdvFriendEdge;
DROP TABLE IF EXISTS dbo.AdvPersonNode;
GO


CREATE TABLE dbo.AdvPersonNode
(
    PersonID INT NOT NULL,

    PersonName NVARCHAR(100)
        NOT NULL
)

AS NODE;
GO


CREATE TABLE dbo.AdvFriendEdge
(
    SinceYear INT,

    RelationshipType NVARCHAR(50)
)

AS EDGE;
GO


INSERT dbo.AdvPersonNode
(
    PersonID,
    PersonName
)

VALUES
(1,N'An'),
(2,N'Bình'),
(3,N'Cường'),
(4,N'Dũng');
GO


INSERT dbo.AdvFriendEdge
(
    $from_id,
    $to_id,
    SinceYear,
    RelationshipType
)

SELECT
    A.$node_id,
    B.$node_id,
    2020,
    N'Friend'

FROM dbo.AdvPersonNode AS A

CROSS JOIN dbo.AdvPersonNode AS B

WHERE
    A.PersonID = 1
    AND
    B.PersonID = 2;
GO


INSERT dbo.AdvFriendEdge
(
    $from_id,
    $to_id,
    SinceYear,
    RelationshipType
)

SELECT
    A.$node_id,
    B.$node_id,
    2022,
    N'Friend'

FROM dbo.AdvPersonNode AS A

CROSS JOIN dbo.AdvPersonNode AS B

WHERE
    A.PersonID = 2
    AND
    B.PersonID = 3;
GO


SELECT

    P1.PersonName
        AS PersonFrom,

    E.RelationshipType,

    E.SinceYear,

    P2.PersonName
        AS PersonTo

FROM
    dbo.AdvPersonNode AS P1,
    dbo.AdvFriendEdge AS E,
    dbo.AdvPersonNode AS P2

WHERE MATCH
(
    P1-(E)->P2
);
GO


/* =====================================================================
   04. HIERARCHYID

   Cấu trúc cây native của SQL Server.
   ===================================================================== */

DROP TABLE IF EXISTS dbo.AdvOrganization;
GO


CREATE TABLE dbo.AdvOrganization
(
    NodeID HIERARCHYID
        NOT NULL
        PRIMARY KEY,

    EmployeeName NVARCHAR(100)
        NOT NULL
);
GO


INSERT dbo.AdvOrganization
(
    NodeID,
    EmployeeName
)

VALUES
(
    hierarchyid::GetRoot(),
    N'CEO'
);
GO


INSERT dbo.AdvOrganization
(
    NodeID,
    EmployeeName
)

VALUES
(
    hierarchyid::Parse('/1/'),
    N'CTO'
),
(
    hierarchyid::Parse('/2/'),
    N'CFO'
),
(
    hierarchyid::Parse('/1/1/'),
    N'Backend Lead'
),
(
    hierarchyid::Parse('/1/2/'),
    N'Frontend Lead'
);
GO


SELECT

    EmployeeName,

    NodeID.ToString()
        AS HierarchyPath,

    NodeID.GetLevel()
        AS HierarchyLevel

FROM dbo.AdvOrganization

ORDER BY NodeID;
GO


SELECT
    EmployeeName,
    NodeID.ToString()

FROM dbo.AdvOrganization

WHERE
    NodeID.IsDescendantOf
    (
        hierarchyid::Parse('/1/')
    ) = 1;
GO


/* =====================================================================
   05. SPARSE COLUMNS + COLUMN SET

   Cấu trúc đặc trưng SQL Server.
   ===================================================================== */

DROP TABLE IF EXISTS dbo.AdvSparseEntity;
GO


CREATE TABLE dbo.AdvSparseEntity
(
    EntityID INT
        NOT NULL
        PRIMARY KEY,

    Phone VARCHAR(50)
        SPARSE NULL,

    Facebook VARCHAR(500)
        SPARSE NULL,

    Github VARCHAR(500)
        SPARSE NULL,

    Age INT
        SPARSE NULL,

    Score DECIMAL(18,2)
        SPARSE NULL,

    AllSparseValues XML
        COLUMN_SET
        FOR ALL_SPARSE_COLUMNS
);
GO


INSERT dbo.AdvSparseEntity
(
    EntityID,
    Github
)
VALUES
(
    1,
    'github.com/example'
);
GO


SELECT *
FROM dbo.AdvSparseEntity;
GO


SELECT
    EntityID,
    AllSparseValues

FROM dbo.AdvSparseEntity;
GO


/* =====================================================================
   06. TABLE TYPE + TABLE VALUED PARAMETER (TVP)

   Rất SQL Server-specific.
   ===================================================================== */

DROP PROCEDURE IF EXISTS dbo.sp_AdvCustomersByIds;
GO


IF TYPE_ID(N'dbo.AdvCustomerIdList') IS NOT NULL

    DROP TYPE dbo.AdvCustomerIdList;
GO


CREATE TYPE dbo.AdvCustomerIdList

AS TABLE
(
    CustomerID INT
        NOT NULL
        PRIMARY KEY
);
GO


CREATE PROCEDURE dbo.sp_AdvCustomersByIds

    @IDs dbo.AdvCustomerIdList
        READONLY

AS

BEGIN

    SET NOCOUNT ON;


    SELECT
        C.CustomerID,
        C.FullName,
        C.City,
        C.Balance

    FROM dbo.Customers AS C

    INNER JOIN @IDs AS I
        ON I.CustomerID =
           C.CustomerID;

END;
GO


DECLARE @IDs dbo.AdvCustomerIdList;

INSERT @IDs
VALUES
(1),
(2),
(4);


EXEC dbo.sp_AdvCustomersByIds
    @IDs = @IDs;
GO


/* =====================================================================
   07. PARTITION FUNCTION + PARTITION SCHEME + PARTITIONED TABLE
   ===================================================================== */

DROP TABLE IF EXISTS dbo.AdvPartitionedOrders;
GO


IF EXISTS
(
    SELECT 1
    FROM sys.partition_schemes
    WHERE name = N'ps_AdvOrderDate'
)

    DROP PARTITION SCHEME ps_AdvOrderDate;
GO


IF EXISTS
(
    SELECT 1
    FROM sys.partition_functions
    WHERE name = N'pf_AdvOrderDate'
)

    DROP PARTITION FUNCTION pf_AdvOrderDate;
GO


CREATE PARTITION FUNCTION
pf_AdvOrderDate(DATE)

AS RANGE RIGHT

FOR VALUES
(
    '2025-01-01',
    '2026-01-01',
    '2027-01-01'
);
GO


CREATE PARTITION SCHEME
ps_AdvOrderDate

AS PARTITION
pf_AdvOrderDate

ALL TO
(
    [PRIMARY]
);
GO


CREATE TABLE dbo.AdvPartitionedOrders
(
    OrderID BIGINT
        NOT NULL,

    OrderDate DATE
        NOT NULL,

    CustomerID INT,

    Amount DECIMAL(18,2),

    CONSTRAINT PK_AdvPartitionedOrders

    PRIMARY KEY CLUSTERED
    (
        OrderID,
        OrderDate
    )
)

ON ps_AdvOrderDate(OrderDate);
GO


INSERT dbo.AdvPartitionedOrders
VALUES
(1,'2024-01-01',1,100),
(2,'2025-01-01',1,200),
(3,'2026-01-01',2,300),
(4,'2027-01-01',2,400),
(5,'2028-01-01',3,500);
GO


SELECT

    $PARTITION.pf_AdvOrderDate(OrderDate)
        AS PartitionNumber,

    COUNT(*)
        AS RowsInPartition,

    MIN(OrderDate)
        AS MinimumDate,

    MAX(OrderDate)
        AS MaximumDate

FROM dbo.AdvPartitionedOrders

GROUP BY
    $PARTITION.pf_AdvOrderDate(OrderDate)

ORDER BY PartitionNumber;
GO


/* =====================================================================
   08. COLUMNSTORE INDEX
   ===================================================================== */

DROP TABLE IF EXISTS dbo.AdvSalesFact;
GO


CREATE TABLE dbo.AdvSalesFact
(
    SalesID BIGINT NOT NULL,

    CustomerID INT,

    ProductID INT,

    Quantity INT,

    Price DECIMAL(18,2),

    SalesDate DATE
);
GO


WITH N AS
(
    SELECT 1 AS Number

    UNION ALL

    SELECT Number + 1

    FROM N

    WHERE Number < 1000
)

INSERT dbo.AdvSalesFact

SELECT
    Number,

    (Number % 100) + 1,

    (Number % 50) + 1,

    (Number % 10) + 1,

    CAST
    (
        (Number % 1000) * 100
        AS DECIMAL(18,2)
    ),

    DATEADD
    (
        DAY,
        Number % 365,
        CAST('2026-01-01' AS DATE)
    )

FROM N

OPTION(MAXRECURSION 0);
GO


CREATE CLUSTERED COLUMNSTORE INDEX
CCI_AdvSalesFact

ON dbo.AdvSalesFact;
GO


SELECT

    ProductID,

    SUM
    (
        Quantity * Price
    ) AS Revenue,

    COUNT_BIG(*)
        AS TotalRows

FROM dbo.AdvSalesFact

GROUP BY ProductID

ORDER BY Revenue DESC;
GO


/* =====================================================================
   09. INDEXED VIEW / MATERIALIZED-LIKE VIEW

   SQL Server gọi là Indexed View.
   ===================================================================== */

SET ANSI_NULLS ON;
SET QUOTED_IDENTIFIER ON;
SET ANSI_PADDING ON;
SET ANSI_WARNINGS ON;
SET CONCAT_NULL_YIELDS_NULL ON;
SET ARITHABORT ON;
SET NUMERIC_ROUNDABORT OFF;
GO


DROP VIEW IF EXISTS dbo.vw_AdvIndexedProducts;
GO


CREATE VIEW dbo.vw_AdvIndexedProducts

WITH SCHEMABINDING

AS

SELECT

    ProductID,

    ProductCode,

    ProductName,

    Price,

    Quantity

FROM dbo.Products;
GO


CREATE UNIQUE CLUSTERED INDEX
CIX_vw_AdvIndexedProducts

ON dbo.vw_AdvIndexedProducts
(
    ProductID
);
GO


SELECT *
FROM dbo.vw_AdvIndexedProducts

WITH (NOEXPAND);
GO


/* =====================================================================
   10. XML STORAGE + PRIMARY XML INDEX + SECONDARY XML INDEX
   ===================================================================== */

DROP TABLE IF EXISTS dbo.AdvXmlDocuments;
GO


CREATE TABLE dbo.AdvXmlDocuments
(
    DocumentID INT
        NOT NULL
        PRIMARY KEY,

    DocumentData XML
        NOT NULL
);
GO


INSERT dbo.AdvXmlDocuments
VALUES
(
    1,

    N'
    <shop>
        <products>
            <product id="1" price="100">
                <name>Nova A</name>
            </product>

            <product id="2" price="200">
                <name>Nova B</name>
            </product>
        </products>
    </shop>'
);
GO


CREATE PRIMARY XML INDEX
PX_AdvXmlDocuments

ON dbo.AdvXmlDocuments
(
    DocumentData
);
GO


CREATE XML INDEX
PX_AdvXmlDocuments_PATH

ON dbo.AdvXmlDocuments
(
    DocumentData
)

USING XML INDEX
PX_AdvXmlDocuments

FOR PATH;
GO


SELECT

    P.N.value
    (
        '@id',
        'INT'
    )
    AS ProductID,

    P.N.value
    (
        '@price',
        'DECIMAL(18,2)'
    )
    AS Price,

    P.N.value
    (
        '(name/text())[1]',
        'NVARCHAR(100)'
    )
    AS ProductName

FROM dbo.AdvXmlDocuments AS X

CROSS APPLY

X.DocumentData.nodes
(
    '/shop/products/product'
)

AS P(N);
GO


/* =====================================================================
   11. SPATIAL / GEOMETRY
   ===================================================================== */

DROP TABLE IF EXISTS dbo.AdvSpatial;
GO


CREATE TABLE dbo.AdvSpatial
(
    LocationID INT
        NOT NULL
        PRIMARY KEY,

    LocationName NVARCHAR(100),

    Shape GEOMETRY
);
GO


INSERT dbo.AdvSpatial
(
    LocationID,
    LocationName,
    Shape
)

VALUES
(
    1,
    N'Point A',
    geometry::Point(10,10,0)
),
(
    2,
    N'Point B',
    geometry::Point(20,20,0)
);
GO


SELECT

    A.LocationName
        AS A,

    B.LocationName
        AS B,

    A.Shape.STDistance(B.Shape)
        AS Distance

FROM dbo.AdvSpatial AS A

CROSS JOIN dbo.AdvSpatial AS B

WHERE
    A.LocationID = 1
    AND
    B.LocationID = 2;
GO


CREATE SPATIAL INDEX
SIX_AdvSpatial

ON dbo.AdvSpatial(Shape)

USING GEOMETRY_GRID

WITH
(
    BOUNDING_BOX =
    (
        0,
        0,
        100,
        100
    )
);
GO


/* =====================================================================
   12. ADVANCED ANALYTIC WINDOW FUNCTIONS
   ===================================================================== */

SELECT

    CustomerID,

    FullName,

    Balance,


    NTILE(4)
    OVER
    (
        ORDER BY Balance
    )
    AS Quartile,


    PERCENT_RANK()
    OVER
    (
        ORDER BY Balance
    )
    AS PercentRank,


    CUME_DIST()
    OVER
    (
        ORDER BY Balance
    )
    AS CumulativeDistribution,


    PERCENTILE_CONT(0.50)

    WITHIN GROUP
    (
        ORDER BY Balance
    )

    OVER()

    AS MedianBalance,


    PERCENTILE_CONT(0.90)

    WITHIN GROUP
    (
        ORDER BY Balance
    )

    OVER()

    AS Percentile90

FROM dbo.Customers;
GO


/* =====================================================================
   13. SQL SERVER 2022 NAMED WINDOW
   Compatibility Level 160
   ===================================================================== */

SELECT

    CustomerID,

    FullName,

    City,

    Balance,


    ROW_NUMBER()
    OVER CustomerWindow
        AS RowNumber,


    SUM(Balance)
    OVER CustomerWindow
        AS RunningBalance

FROM dbo.Customers

WINDOW CustomerWindow AS
(
    PARTITION BY City

    ORDER BY
        Balance DESC,
        CustomerID

    ROWS BETWEEN
        UNBOUNDED PRECEDING
        AND CURRENT ROW
);
GO


/* =====================================================================
   14. JSON MODIFY + NESTED JSON
   ===================================================================== */

DECLARE @AdvancedJson NVARCHAR(MAX) =
N'
{
    "customer": {
        "id": 1,
        "name": "Nova",
        "settings": {
            "language": "vi",
            "theme": "dark"
        },
        "orders": [
            {
                "id": 1001,
                "total": 100000
            },
            {
                "id": 1002,
                "total": 200000
            }
        ]
    }
}
';


SET @AdvancedJson =
    JSON_MODIFY
    (
        @AdvancedJson,

        '$.customer.settings.language',

        'en'
    );


SET @AdvancedJson =
    JSON_MODIFY
    (
        @AdvancedJson,

        '$.customer.vip',

        CAST(1 AS BIT)
    );


SELECT

    JSON_VALUE
    (
        @AdvancedJson,
        '$.customer.name'
    ) AS CustomerName,

    JSON_VALUE
    (
        @AdvancedJson,
        '$.customer.settings.language'
    ) AS Language,

    JSON_QUERY
    (
        @AdvancedJson,
        '$.customer.orders'
    ) AS Orders,

    @AdvancedJson
        AS FullJson;
GO


/* =====================================================================
   15. OPENJSON MULTI LEVEL + APPLY
   ===================================================================== */

DECLARE @OrdersJson NVARCHAR(MAX) =
N'
[
    {
        "customerId": 1,
        "orders":
        [
            {
                "id": 100,
                "items":
                [
                    {"productId":1,"qty":2},
                    {"productId":2,"qty":3}
                ]
            },
            {
                "id": 101,
                "items":
                [
                    {"productId":3,"qty":1}
                ]
            }
        ]
    }
]
';


SELECT

    CustomerData.CustomerID,

    OrderData.OrderID,

    ItemData.ProductID,

    ItemData.Quantity

FROM OPENJSON(@OrdersJson)

WITH
(
    CustomerID INT
        '$.customerId',

    Orders NVARCHAR(MAX)
        '$.orders'
        AS JSON
)

AS CustomerData


CROSS APPLY

OPENJSON(CustomerData.Orders)

WITH
(
    OrderID INT
        '$.id',

    Items NVARCHAR(MAX)
        '$.items'
        AS JSON
)

AS OrderData


CROSS APPLY

OPENJSON(OrderData.Items)

WITH
(
    ProductID INT
        '$.productId',

    Quantity INT
        '$.qty'
)

AS ItemData;
GO


/* =====================================================================
   16. ROW LEVEL SECURITY

   SQL Server 2016+
   ===================================================================== */

IF EXISTS
(
    SELECT 1
    FROM sys.security_policies

    WHERE name =
        N'AdvTenantSecurityPolicy'
)

DROP SECURITY POLICY
Security.AdvTenantSecurityPolicy;
GO


DROP FUNCTION IF EXISTS
Security.fn_AdvTenantAccess;
GO


DROP TABLE IF EXISTS
dbo.AdvTenantData;
GO


IF SCHEMA_ID(N'Security') IS NULL
BEGIN

    EXEC
    (
        N'CREATE SCHEMA Security'
    );

END;
GO


CREATE TABLE dbo.AdvTenantData
(
    DataID INT IDENTITY(1,1)
        PRIMARY KEY,

    TenantID INT NOT NULL,

    SecretData NVARCHAR(500)
        NOT NULL
);
GO


INSERT dbo.AdvTenantData
(
    TenantID,
    SecretData
)

VALUES
(1,N'Tenant 1 Data A'),
(1,N'Tenant 1 Data B'),
(2,N'Tenant 2 Data A'),
(3,N'Tenant 3 Data A');
GO


CREATE FUNCTION
Security.fn_AdvTenantAccess
(
    @TenantID INT
)

RETURNS TABLE

WITH SCHEMABINDING

AS

RETURN
(
    SELECT
        1 AS AccessResult

    WHERE

        @TenantID =

        CONVERT
        (
            INT,

            SESSION_CONTEXT
            (
                N'TenantID'
            )
        )
);
GO


CREATE SECURITY POLICY
Security.AdvTenantSecurityPolicy

ADD FILTER PREDICATE

Security.fn_AdvTenantAccess
(
    TenantID
)

ON dbo.AdvTenantData

WITH
(
    STATE = ON
);
GO


EXEC sys.sp_set_session_context

    @key =
        N'TenantID',

    @value =
        1;
GO


SELECT *
FROM dbo.AdvTenantData;
GO


EXEC sys.sp_set_session_context

    @key =
        N'TenantID',

    @value =
        2;
GO


SELECT *
FROM dbo.AdvTenantData;
GO


/* =====================================================================
   17. DYNAMIC PIVOT

   SQL sinh SQL.
   ===================================================================== */

DECLARE
    @Columns NVARCHAR(MAX),
    @DynamicSQL NVARCHAR(MAX);


SELECT

    @Columns =

    STRING_AGG
    (
        CAST
        (
            QUOTENAME(City)
            AS NVARCHAR(MAX)
        ),

        N','
    )

FROM
(
    SELECT DISTINCT City

    FROM dbo.Customers

    WHERE City IS NOT NULL
)

AS Cities;


SET @DynamicSQL =
N'
SELECT *
FROM
(
    SELECT
        City,
        Balance

    FROM dbo.Customers
)
AS SourceData

PIVOT
(
    SUM(Balance)

    FOR City IN
    (
        ' + @Columns + N'
    )
)
AS PivotResult;
';


SELECT
    @DynamicSQL
    AS GeneratedSQL;


EXEC sys.sp_executesql
    @DynamicSQL;
GO


/* =====================================================================
   18. CURSOR + PROCEDURAL FLOW
   ===================================================================== */

DECLARE
    @CustomerID INT,

    @CustomerName NVARCHAR(150);


DECLARE CustomerCursor

CURSOR LOCAL FAST_FORWARD

FOR

SELECT
    CustomerID,
    FullName

FROM dbo.Customers

ORDER BY CustomerID;


OPEN CustomerCursor;


FETCH NEXT
FROM CustomerCursor

INTO
    @CustomerID,
    @CustomerName;


WHILE @@FETCH_STATUS = 0
BEGIN

    SELECT
        @CustomerID
            AS CustomerID,

        @CustomerName
            AS CustomerName;


    FETCH NEXT
    FROM CustomerCursor

    INTO
        @CustomerID,
        @CustomerName;

END;


CLOSE CustomerCursor;

DEALLOCATE CustomerCursor;
GO


/* =====================================================================
   19. SEQUENCE + ORDERED NEXT VALUE FOR
   ===================================================================== */

DROP SEQUENCE IF EXISTS
dbo.AdvOrderedSequence;
GO


CREATE SEQUENCE
dbo.AdvOrderedSequence

AS BIGINT

START WITH 100000

INCREMENT BY 10

CACHE 20;
GO


SELECT

    CustomerID,

    FullName,

    NEXT VALUE FOR
    dbo.AdvOrderedSequence

    OVER
    (
        ORDER BY
            CustomerID
    )

    AS GeneratedNumber

FROM dbo.Customers;
GO


/* =====================================================================
   20. TRANSACTION + XACT_ABORT + TRY/CATCH + SAVEPOINT
   ===================================================================== */

SET XACT_ABORT ON;
GO


BEGIN TRY

    BEGIN TRANSACTION;


    UPDATE dbo.Customers

    SET Balance =
        Balance + 1000

    WHERE CustomerID = 1;


    SAVE TRANSACTION
        AfterFirstUpdate;


    UPDATE dbo.Customers

    SET Balance =
        Balance + 2000

    WHERE CustomerID = 2;


    IF
    (
        SELECT SUM(Balance)
        FROM dbo.Customers
    ) > 999999999999

    BEGIN

        THROW
            51000,
            'Business rule failed',
            1;

    END;


    COMMIT TRANSACTION;

END TRY


BEGIN CATCH

    DECLARE
        @TransactionState INT =
            XACT_STATE();


    IF @TransactionState <> 0
    BEGIN

        ROLLBACK TRANSACTION;

    END;


    SELECT

        ERROR_NUMBER()
            AS ErrorNumber,

        ERROR_MESSAGE()
            AS ErrorMessage,

        ERROR_LINE()
            AS ErrorLine,

        ERROR_STATE()
            AS ErrorState,

        @TransactionState
            AS TransactionState;

END CATCH;
GO


SET XACT_ABORT OFF;
GO


/* =====================================================================
   21. COMPLEX RECURSIVE TREE + CYCLE PROTECTION
   ===================================================================== */

DROP TABLE IF EXISTS dbo.AdvTree;
GO


CREATE TABLE dbo.AdvTree
(
    NodeID INT
        PRIMARY KEY,

    ParentID INT NULL,

    NodeName NVARCHAR(100)
        NOT NULL
);
GO


INSERT dbo.AdvTree
VALUES
(1,NULL,N'Root'),
(2,1,N'A'),
(3,1,N'B'),
(4,2,N'A.1'),
(5,2,N'A.2'),
(6,4,N'A.1.1'),
(7,3,N'B.1');
GO


WITH TreeCTE AS
(
    SELECT

        NodeID,

        ParentID,

        NodeName,

        0 AS Depth,

        CAST
        (
            '/' +
            CAST(NodeID AS VARCHAR(20)) +
            '/'

            AS VARCHAR(MAX)
        )

        AS TraversedPath,


        CAST
        (
            NodeName

            AS NVARCHAR(MAX)
        )

        AS DisplayPath

    FROM dbo.AdvTree

    WHERE ParentID IS NULL


    UNION ALL


    SELECT

        Child.NodeID,

        Child.ParentID,

        Child.NodeName,

        Parent.Depth + 1,


        Parent.TraversedPath
        +
        CAST
        (
            Child.NodeID
            AS VARCHAR(20)
        )
        +
        '/',


        Parent.DisplayPath
        +
        N' -> '
        +
        Child.NodeName


    FROM dbo.AdvTree AS Child

    INNER JOIN TreeCTE AS Parent

        ON Child.ParentID =
           Parent.NodeID


    WHERE

        CHARINDEX
        (
            '/' +
            CAST
            (
                Child.NodeID
                AS VARCHAR(20)
            )
            +
            '/',

            Parent.TraversedPath
        ) = 0
)


SELECT
    *

FROM TreeCTE

ORDER BY
    DisplayPath

OPTION(MAXRECURSION 32767);
GO


/* =====================================================================
   22. MONSTER ANALYTIC QUERY

   Multiple CTE
   + recursive
   + aggregation
   + percentile
   + ranking
   + APPLY
   + correlated subquery
   + EXISTS
   + nested CASE
   + window frame
   ===================================================================== */

WITH
CustomerBase AS
(
    SELECT

        CustomerID,

        FullName,

        City,

        Balance,

        CreatedAt

    FROM dbo.Customers

    WHERE Balance >= 0
),

CityStatistics AS
(
    SELECT

        City,

        COUNT_BIG(*)
            AS CustomerCount,

        SUM(Balance)
            AS CityBalance,

        AVG(Balance)
            AS AverageBalance,

        MIN(Balance)
            AS MinimumBalance,

        MAX(Balance)
            AS MaximumBalance

    FROM CustomerBase

    GROUP BY City
),

CustomerAnalytics AS
(
    SELECT

        C.*,

        S.CustomerCount,

        S.CityBalance,

        S.AverageBalance,

        S.MinimumBalance,

        S.MaximumBalance,


        ROW_NUMBER()
        OVER
        (
            PARTITION BY C.City

            ORDER BY
                C.Balance DESC,
                C.CustomerID
        )

        AS CityRowNumber,


        DENSE_RANK()
        OVER
        (
            ORDER BY C.Balance DESC
        )

        AS GlobalBalanceRank,


        LAG
        (
            C.Balance,
            1,
            0
        )

        OVER
        (
            PARTITION BY C.City

            ORDER BY C.Balance
        )

        AS PreviousBalance,


        LEAD
        (
            C.Balance,
            1,
            0
        )

        OVER
        (
            PARTITION BY C.City

            ORDER BY C.Balance
        )

        AS NextBalance,


        SUM(C.Balance)

        OVER
        (
            PARTITION BY C.City

            ORDER BY
                C.CustomerID

            ROWS BETWEEN
                UNBOUNDED PRECEDING
                AND CURRENT ROW
        )

        AS RunningCityBalance,


        AVG(C.Balance)

        OVER
        (
            ORDER BY C.CustomerID

            ROWS BETWEEN
                2 PRECEDING
                AND CURRENT ROW
        )

        AS MovingAverage3,


        PERCENTILE_CONT(0.5)

        WITHIN GROUP
        (
            ORDER BY C.Balance
        )

        OVER
        (
            PARTITION BY C.City
        )

        AS CityMedianBalance


    FROM CustomerBase AS C


    INNER JOIN CityStatistics AS S

        ON
        (
            C.City = S.City

            OR

            (
                C.City IS NULL
                AND
                S.City IS NULL
            )
        )
)


SELECT TOP (100)

    A.CustomerID,

    A.FullName,

    A.City,

    A.Balance,

    A.CustomerCount,

    A.CityBalance,

    A.AverageBalance,

    A.CityMedianBalance,

    A.CityRowNumber,

    A.GlobalBalanceRank,

    A.PreviousBalance,

    A.NextBalance,

    A.RunningCityBalance,

    A.MovingAverage3,


    Classification.CustomerLevel,


    Calculated.BalanceWithBonus,


    (
        SELECT COUNT_BIG(*)

        FROM dbo.Customers AS OtherCustomer

        WHERE

            OtherCustomer.City =
            A.City

            AND

            OtherCustomer.Balance >
            A.Balance

    )

    AS RicherCustomersInSameCity,


    CASE

        WHEN EXISTS
        (
            SELECT 1

            FROM dbo.Orders AS O

            WHERE
                O.CustomerID =
                A.CustomerID
        )

        THEN CAST(1 AS BIT)

        ELSE CAST(0 AS BIT)

    END

    AS HasOrders


FROM CustomerAnalytics AS A


CROSS APPLY
(
    SELECT

        CASE

            WHEN A.Balance >=
                 A.CityMedianBalance * 2

            THEN N'ULTRA VIP'


            WHEN A.Balance >
                 A.AverageBalance

            THEN

                CASE

                    WHEN
                        A.CityRowNumber = 1

                    THEN N'CITY TOP'

                    ELSE N'ABOVE AVERAGE'

                END


            WHEN A.Balance =
                 A.AverageBalance

            THEN N'AVERAGE'


            ELSE N'BELOW AVERAGE'

        END

        AS CustomerLevel
)

AS Classification


CROSS APPLY
(
    SELECT

        A.Balance
        *
        CASE

            WHEN Classification.CustomerLevel =
                 N'ULTRA VIP'

                THEN 1.20

            WHEN Classification.CustomerLevel =
                 N'CITY TOP'

                THEN 1.15

            ELSE 1.05

        END

        AS BalanceWithBonus
)

AS Calculated


WHERE

    A.Balance > 0

    AND

    A.CustomerID IN
    (
        SELECT CustomerID

        FROM dbo.Customers

        WHERE CreatedAt IS NOT NULL
    )


ORDER BY

    A.City,

    A.Balance DESC,

    A.CustomerID

OPTION
(
    RECOMPILE
);
GO
"""

if __name__ == '__main__':
    batches = [b.strip() for b in re.split(r'(?im)^\s*GO\s*;?\s*$', ultra_script) if b.strip()]
    print(f"Total batches to test: {len(batches)}")
    for idx, b in enumerate(batches, 1):
        r = requests.post('http://127.0.0.1:8787/v1/admin/databases/test_db/execute', json={'sql': b})
        if r.status_code != 200:
            print(f"Batch {idx} FAILED (HTTP {r.status_code}): {r.text[:300]}")
            print(f"SQL snippet:\n{b[:200]}")
            break
        else:
            print(f"Batch {idx} passed!")
    else:
        print("All batches passed!")
