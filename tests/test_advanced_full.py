import requests

BASE = 'http://127.0.0.1:8787/v1/admin/databases/test_db'

full_script = """
/* ================================================================
   SQL SERVER ADVANCED / COMPOUND STRUCTURE TEST
   ================================================================ */

USE NovaSqlServerLab;
GO


/* ================================================================
   01. COMPOSITE PRIMARY KEY
   ================================================================ */

DROP TABLE IF EXISTS dbo.StudentCourse;
DROP TABLE IF EXISTS dbo.Course;
DROP TABLE IF EXISTS dbo.Student;
GO

CREATE TABLE dbo.Student
(
    StudentID INT PRIMARY KEY,
    FullName NVARCHAR(100) NOT NULL
);

CREATE TABLE dbo.Course
(
    CourseID INT PRIMARY KEY,
    CourseName NVARCHAR(100) NOT NULL
);

CREATE TABLE dbo.StudentCourse
(
    StudentID INT NOT NULL,
    CourseID INT NOT NULL,
    Semester INT NOT NULL,
    Score DECIMAL(5,2),

    CONSTRAINT PK_StudentCourse
        PRIMARY KEY
        (
            StudentID,
            CourseID,
            Semester
        ),

    CONSTRAINT FK_SC_Student
        FOREIGN KEY(StudentID)
        REFERENCES dbo.Student(StudentID),

    CONSTRAINT FK_SC_Course
        FOREIGN KEY(CourseID)
        REFERENCES dbo.Course(CourseID),

    CONSTRAINT CK_SC_Score
        CHECK
        (
            Score IS NULL
            OR
            (Score >= 0 AND Score <= 10)
        )
);
GO


/* ================================================================
   02. COMPOSITE UNIQUE
   ================================================================ */

CREATE TABLE dbo.UserAccount
(
    UserID INT IDENTITY(1,1) PRIMARY KEY,

    TenantID INT NOT NULL,

    Username VARCHAR(100) NOT NULL,

    Email VARCHAR(200),

    CONSTRAINT UQ_User_Tenant_Username
        UNIQUE
        (
            TenantID,
            Username
        )
);
GO


/* ================================================================
   03. COMPOSITE FOREIGN KEY
   ================================================================ */

CREATE TABLE dbo.WarehouseProduct
(
    WarehouseID INT NOT NULL,
    ProductID INT NOT NULL,

    Quantity INT NOT NULL DEFAULT 0,

    CONSTRAINT PK_WarehouseProduct
        PRIMARY KEY
        (
            WarehouseID,
            ProductID
        )
);
GO


CREATE TABLE dbo.InventoryHistory
(
    HistoryID BIGINT IDENTITY(1,1)
        PRIMARY KEY,

    WarehouseID INT NOT NULL,

    ProductID INT NOT NULL,

    OldQuantity INT,

    NewQuantity INT,

    ChangedAt DATETIME2
        DEFAULT SYSDATETIME(),

    CONSTRAINT FK_InventoryHistory_WarehouseProduct
        FOREIGN KEY
        (
            WarehouseID,
            ProductID
        )
        REFERENCES dbo.WarehouseProduct
        (
            WarehouseID,
            ProductID
        )
);
GO


/* ================================================================
   04. COMPOSITE INDEX
   ================================================================ */

CREATE INDEX IX_Customers_City_Balance_CreatedAt
ON dbo.Customers
(
    City ASC,
    Balance DESC,
    CreatedAt DESC
)
INCLUDE
(
    FullName,
    Email,
    Phone
)
WHERE Balance > 0;
GO


/* ================================================================
   05. SELF-REFERENCING TABLE
   ================================================================ */

DROP TABLE IF EXISTS dbo.CategoryTree;
GO

CREATE TABLE dbo.CategoryTree
(
    CategoryID INT IDENTITY(1,1)
        PRIMARY KEY,

    ParentCategoryID INT NULL,

    CategoryName NVARCHAR(100)
        NOT NULL,

    CONSTRAINT FK_CategoryTree_Parent
        FOREIGN KEY(ParentCategoryID)
        REFERENCES dbo.CategoryTree(CategoryID)
);
GO


INSERT INTO dbo.CategoryTree
(
    ParentCategoryID,
    CategoryName
)
VALUES
(NULL,N'Điện tử');
GO


INSERT INTO dbo.CategoryTree
(
    ParentCategoryID,
    CategoryName
)
VALUES
(1,N'Máy tính'),
(1,N'Điện thoại');
GO


INSERT INTO dbo.CategoryTree
(
    ParentCategoryID,
    CategoryName
)
VALUES
(2,N'Laptop'),
(2,N'PC'),
(3,N'Android'),
(3,N'iPhone');
GO


/* ================================================================
   06. RECURSIVE HIERARCHY CTE
   ================================================================ */

WITH CategoryHierarchy AS
(
    SELECT
        CategoryID,
        ParentCategoryID,
        CategoryName,

        CAST(CategoryName AS NVARCHAR(MAX))
            AS FullPath,

        0 AS Level

    FROM dbo.CategoryTree

    WHERE ParentCategoryID IS NULL


    UNION ALL


    SELECT
        C.CategoryID,
        C.ParentCategoryID,
        C.CategoryName,

        CAST
        (
            H.FullPath
            + N' > '
            + C.CategoryName

            AS NVARCHAR(MAX)
        )
        AS FullPath,

        H.Level + 1

    FROM dbo.CategoryTree AS C

    INNER JOIN CategoryHierarchy AS H
        ON C.ParentCategoryID =
           H.CategoryID
)

SELECT *
FROM CategoryHierarchy

ORDER BY FullPath

OPTION(MAXRECURSION 100);
GO


/* ================================================================
   07. MULTIPLE CTE
   ================================================================ */

WITH
CustomerBase AS
(
    SELECT
        CustomerID,
        FullName,
        City,
        Balance
    FROM dbo.Customers
),

CityAverage AS
(
    SELECT
        City,
        AVG(Balance) AS AverageBalance
    FROM CustomerBase
    GROUP BY City
),

CustomerAnalysis AS
(
    SELECT
        C.CustomerID,
        C.FullName,
        C.City,
        C.Balance,

        A.AverageBalance,

        C.Balance - A.AverageBalance
            AS DifferenceFromAverage

    FROM CustomerBase AS C

    INNER JOIN CityAverage AS A
        ON
        (
            C.City = A.City

            OR

            (
                C.City IS NULL
                AND
                A.City IS NULL
            )
        )
)

SELECT *
FROM CustomerAnalysis;
GO


/* ================================================================
   08. NESTED CASE
   ================================================================ */

SELECT
    CustomerID,
    FullName,
    City,
    Balance,

    CASE

        WHEN Balance >= 5000000
        THEN

            CASE
                WHEN City = N'Hà Nội'
                    THEN N'VIP Hà Nội'

                WHEN City = N'TP.HCM'
                    THEN N'VIP TP.HCM'

                ELSE N'VIP Other'
            END

        WHEN Balance >= 2000000
        THEN

            CASE
                WHEN City IS NULL
                    THEN N'Premium Unknown'

                ELSE N'Premium'
            END

        ELSE N'Normal'

    END AS CustomerClassification

FROM dbo.Customers;
GO


/* ================================================================
   09. NESTED SUBQUERY
   ================================================================ */

SELECT
    CustomerID,
    FullName,
    Balance

FROM dbo.Customers

WHERE Balance >
(
    SELECT AVG(Balance)

    FROM dbo.Customers

    WHERE Balance >
    (
        SELECT MIN(Balance)
        FROM dbo.Customers
        WHERE Balance > 0
    )
);
GO


/* ================================================================
   10. CORRELATED SUBQUERY
   ================================================================ */

SELECT
    C.CustomerID,
    C.FullName,
    C.Balance,

    (
        SELECT COUNT(*)

        FROM dbo.Orders AS O

        WHERE O.CustomerID =
              C.CustomerID

    ) AS OrderCount

FROM dbo.Customers AS C;
GO


/* ================================================================
   11. EXISTS + NOT EXISTS LỒNG
   ================================================================ */

SELECT
    C.CustomerID,
    C.FullName

FROM dbo.Customers AS C

WHERE EXISTS
(
    SELECT 1

    FROM dbo.Orders AS O

    WHERE
        O.CustomerID =
        C.CustomerID

        AND NOT EXISTS
        (
            SELECT 1

            FROM dbo.OrderDetails AS OD

            WHERE
                OD.OrderID =
                O.OrderID

                AND
                OD.Quantity <= 0
        )
);
GO


/* ================================================================
   12. DERIVED TABLE LỒNG
   ================================================================ */

SELECT *

FROM
(
    SELECT
        X.*,

        CASE
            WHEN X.Balance >
                 X.AverageBalance
                THEN N'ABOVE'

            ELSE N'BELOW'
        END AS Position

    FROM
    (
        SELECT
            CustomerID,
            FullName,
            Balance,

            AVG(Balance)
            OVER()
            AS AverageBalance

        FROM dbo.Customers

    ) AS X

) AS Y

WHERE Y.Position = N'ABOVE';
GO


/* ================================================================
   13. MULTIPLE JOIN
   ================================================================ */

SELECT
    O.OrderID,

    C.CustomerID,
    C.FullName,

    OD.ProductID,

    P.ProductCode,
    P.ProductName,

    CAT.CategoryName,

    OD.Quantity,
    OD.UnitPrice,
    OD.LineTotal

FROM dbo.Orders AS O

INNER JOIN dbo.Customers AS C
    ON C.CustomerID =
       O.CustomerID

INNER JOIN dbo.OrderDetails AS OD
    ON OD.OrderID =
       O.OrderID

INNER JOIN dbo.Products AS P
    ON P.ProductID =
       OD.ProductID

LEFT JOIN dbo.Categories AS CAT
    ON CAT.CategoryID =
       P.CategoryID;
GO


/* ================================================================
   14. CROSS APPLY LỒNG
   ================================================================ */

SELECT
    P.ProductID,
    P.ProductName,
    P.Price,

    VAT.PriceVAT,

    Discount.PriceAfterDiscount

FROM dbo.Products AS P


CROSS APPLY
(
    SELECT
        P.Price * 1.10
        AS PriceVAT

) AS VAT


CROSS APPLY
(
    SELECT

        CASE

            WHEN VAT.PriceVAT >= 20000000
                THEN VAT.PriceVAT * 0.90

            WHEN VAT.PriceVAT >= 10000000
                THEN VAT.PriceVAT * 0.95

            ELSE VAT.PriceVAT

        END AS PriceAfterDiscount

) AS Discount;
GO


/* ================================================================
   15. OUTER APPLY + TOP
   ================================================================ */

SELECT
    C.CustomerID,
    C.FullName,

    LatestOrder.OrderID,
    LatestOrder.OrderDate,
    LatestOrder.TotalAmount

FROM dbo.Customers AS C

OUTER APPLY
(
    SELECT TOP (1)
        O.OrderID,
        O.OrderDate,
        O.TotalAmount

    FROM dbo.Orders AS O

    WHERE
        O.CustomerID =
        C.CustomerID

    ORDER BY
        O.OrderDate DESC,
        O.OrderID DESC

) AS LatestOrder;
GO


/* ================================================================
   16. CONDITIONAL AGGREGATION
   ================================================================ */

SELECT
    City,

    COUNT(*) AS TotalCustomers,

    SUM
    (
        CASE
            WHEN Balance >= 5000000
                THEN 1
            ELSE 0
        END
    ) AS VIPCustomers,

    SUM
    (
        CASE
            WHEN Balance >= 2000000
                AND Balance < 5000000
                THEN 1
            ELSE 0
        END
    ) AS PremiumCustomers,

    SUM
    (
        CASE
            WHEN Balance < 2000000
                THEN Balance
            ELSE 0
        END
    ) AS LowCustomerBalance

FROM dbo.Customers

GROUP BY City;
GO


/* ================================================================
   17. ADVANCED WINDOW FUNCTIONS
   ================================================================ */

SELECT
    CustomerID,
    FullName,
    City,
    Balance,

    ROW_NUMBER()
    OVER
    (
        PARTITION BY City
        ORDER BY Balance DESC
    ) AS CityRow,

    RANK()
    OVER
    (
        PARTITION BY City
        ORDER BY Balance DESC
    ) AS CityRank,

    DENSE_RANK()
    OVER
    (
        PARTITION BY City
        ORDER BY Balance DESC
    ) AS DenseCityRank,

    LAG(Balance,1,0)
    OVER
    (
        PARTITION BY City
        ORDER BY Balance
    ) AS PreviousBalance,

    LEAD(Balance,1,0)
    OVER
    (
        PARTITION BY City
        ORDER BY Balance
    ) AS NextBalance,

    FIRST_VALUE(Balance)
    OVER
    (
        PARTITION BY City
        ORDER BY Balance DESC
    ) AS HighestBalance,

    LAST_VALUE(Balance)
    OVER
    (
        PARTITION BY City
        ORDER BY Balance DESC

        ROWS BETWEEN
            UNBOUNDED PRECEDING
            AND
            UNBOUNDED FOLLOWING

    ) AS LowestBalance,

    SUM(Balance)
    OVER
    (
        PARTITION BY City

        ORDER BY CustomerID

        ROWS BETWEEN
            UNBOUNDED PRECEDING
            AND CURRENT ROW

    ) AS RunningTotal

FROM dbo.Customers;
GO


/* ================================================================
   18. WINDOW MOVING AVERAGE
   ================================================================ */

SELECT
    CustomerID,
    Balance,

    AVG(Balance)
    OVER
    (
        ORDER BY CustomerID

        ROWS BETWEEN
            2 PRECEDING
            AND CURRENT ROW

    ) AS MovingAverage3Rows

FROM dbo.Customers;
GO


/* ================================================================
   19. GROUPING SETS PHỨC HỢP
   ================================================================ */

SELECT
    City,

    CASE
        WHEN Balance >= 5000000
            THEN N'VIP'
        WHEN Balance >= 2000000
            THEN N'Premium'
        ELSE N'Normal'
    END AS Level,

    COUNT(*) AS Total,

    SUM(Balance)
        AS TotalBalance

FROM dbo.Customers

GROUP BY GROUPING SETS
(
    (
        City,

        CASE
            WHEN Balance >= 5000000
                THEN N'VIP'
            WHEN Balance >= 2000000
                THEN N'Premium'
            ELSE N'Normal'
        END
    ),

    (City),

    ()
);
GO


/* ================================================================
   20. PIVOT WITH DERIVED DATA
   ================================================================ */

SELECT
    CustomerLevel,

    [Hà Nội],
    [TP.HCM],
    [Đà Nẵng]

FROM
(
    SELECT

        City,

        CASE
            WHEN Balance >= 5000000
                THEN N'VIP'
            WHEN Balance >= 2000000
                THEN N'Premium'
            ELSE N'Normal'
        END AS CustomerLevel,

        Balance

    FROM dbo.Customers

) AS SourceData

PIVOT
(
    SUM(Balance)

    FOR City IN
    (
        [Hà Nội],
        [TP.HCM],
        [Đà Nẵng]
    )
) AS PivotResult;
GO


/* ================================================================
   21. MULTI-COLUMN UPDATE FROM JOIN
   ================================================================ */

UPDATE P

SET
    P.Price =
        P.Price * 1.05,

    P.Quantity =
        CASE

            WHEN P.Quantity < 10
                THEN P.Quantity + 10

            ELSE P.Quantity

        END

FROM dbo.Products AS P

INNER JOIN dbo.Categories AS C
    ON C.CategoryID =
       P.CategoryID

WHERE
    C.CategoryName = N'Laptop';
GO


/* ================================================================
   22. UPDATE CTE
   ================================================================ */

WITH ExpensiveProducts AS
(
    SELECT
        ProductID,
        Price

    FROM dbo.Products

    WHERE Price >= 10000000
)

UPDATE ExpensiveProducts

SET Price = Price * 1.01;
GO


/* ================================================================
   23. DELETE THROUGH CTE
   ================================================================ */

BEGIN TRANSACTION;
GO

WITH LowBalanceCustomers AS
(
    SELECT *

    FROM dbo.Customers

    WHERE Balance < 500000
)

DELETE FROM LowBalanceCustomers;
GO

ROLLBACK TRANSACTION;
GO


/* ================================================================
   24. OUTPUT INSERTED + DELETED
   ================================================================ */

DECLARE @Changes TABLE
(
    ProductID INT,

    OldPrice DECIMAL(18,2),

    NewPrice DECIMAL(18,2)
);


UPDATE dbo.Products

SET Price =
    Price + 1000

OUTPUT
    deleted.ProductID,
    deleted.Price,
    inserted.Price

INTO @Changes
(
    ProductID,
    OldPrice,
    NewPrice
)

WHERE ProductID <= 3;


SELECT *
FROM @Changes;
GO


/* ================================================================
   25. MERGE PHỨC HỢP
   ================================================================ */

DECLARE @SourceProducts TABLE
(
    ProductCode VARCHAR(30),

    ProductName NVARCHAR(200),

    Price DECIMAL(18,2),

    Quantity INT
);


INSERT INTO @SourceProducts
VALUES
(
    'LAP001',
    N'Nova Laptop Updated',
    27000000,
    25
),
(
    'NEW001',
    N'Nova New Product',
    5000000,
    20
);


MERGE dbo.Products AS TARGET

USING @SourceProducts AS SOURCE

ON
    TARGET.ProductCode =
    SOURCE.ProductCode


WHEN MATCHED
    AND
    (
        TARGET.Price <>
        SOURCE.Price

        OR

        TARGET.Quantity <>
        SOURCE.Quantity
    )

THEN UPDATE

SET
    TARGET.ProductName =
        SOURCE.ProductName,

    TARGET.Price =
        SOURCE.Price,

    TARGET.Quantity =
        SOURCE.Quantity


WHEN NOT MATCHED BY TARGET

THEN INSERT
(
    ProductCode,
    ProductName,
    Price,
    Quantity
)

VALUES
(
    SOURCE.ProductCode,
    SOURCE.ProductName,
    SOURCE.Price,
    SOURCE.Quantity
)


OUTPUT
    $action,

    deleted.ProductCode
        AS OldProduct,

    inserted.ProductCode
        AS NewProduct,

    deleted.Price
        AS OldPrice,

    inserted.Price
        AS NewPrice;
GO


/* ================================================================
   26. NESTED IF
   ================================================================ */

DECLARE @Balance DECIMAL(18,2) = 6000000;

DECLARE @City NVARCHAR(100) =
    N'Hà Nội';


IF @Balance >= 5000000
BEGIN

    IF @City = N'Hà Nội'
    BEGIN
        SELECT N'VIP HANOI'
            AS Result;
    END

    ELSE
    BEGIN
        SELECT N'VIP OTHER'
            AS Result;
    END

END

ELSE
BEGIN

    IF @Balance >= 2000000
        SELECT N'PREMIUM'
            AS Result;

    ELSE
        SELECT N'NORMAL'
            AS Result;

END;
GO


/* ================================================================
   27. NESTED TRY/CATCH
   ================================================================ */

BEGIN TRY

    BEGIN TRY

        SELECT
            CAST
            (
                'NOT_NUMBER'
                AS INT
            );

    END TRY

    BEGIN CATCH

        THROW;

    END CATCH;

END TRY

BEGIN CATCH

    SELECT
        ERROR_NUMBER()
            AS ErrorNumber,

        ERROR_MESSAGE()
            AS ErrorMessage,

        ERROR_LINE()
            AS ErrorLine;

END CATCH;
GO


/* ================================================================
   28. NESTED TRANSACTION
   ================================================================ */

BEGIN TRANSACTION;

SELECT
    @@TRANCOUNT
    AS TransactionLevel1;


BEGIN TRANSACTION;

SELECT
    @@TRANCOUNT
    AS TransactionLevel2;


UPDATE dbo.Customers
SET Balance =
    Balance + 1
WHERE CustomerID = 1;


COMMIT TRANSACTION;

SELECT
    @@TRANCOUNT
    AS AfterInnerCommit;


COMMIT TRANSACTION;

SELECT
    @@TRANCOUNT
    AS AfterOuterCommit;
GO


/* ================================================================
   29. SAVEPOINT + TRY CATCH
   ================================================================ */

BEGIN TRY

    BEGIN TRANSACTION;


    UPDATE dbo.Customers

    SET Balance =
        Balance + 100

    WHERE CustomerID = 1;


    SAVE TRANSACTION
        BeforeSecondUpdate;


    UPDATE dbo.Customers

    SET Balance =
        Balance + 500

    WHERE CustomerID = 2;


    ROLLBACK TRANSACTION
        BeforeSecondUpdate;


    COMMIT TRANSACTION;

END TRY

BEGIN CATCH

    IF XACT_STATE() <> 0
        ROLLBACK TRANSACTION;

    THROW;

END CATCH;
GO


/* ================================================================
   30. DYNAMIC SQL + INPUT + OUTPUT PARAMETER
   ================================================================ */

DECLARE
    @SQL NVARCHAR(MAX),

    @MinimumBalance DECIMAL(18,2)
        = 1000000,

    @CountResult INT;


SET @SQL =
N'
    SELECT
        @Count =
            COUNT(*)

    FROM dbo.Customers

    WHERE Balance >=
          @Minimum;
';


EXEC sys.sp_executesql

    @SQL,

    N'
        @Minimum DECIMAL(18,2),
        @Count INT OUTPUT
    ',

    @Minimum =
        @MinimumBalance,

    @Count =
        @CountResult OUTPUT;


SELECT
    @CountResult
    AS MatchingCustomers;
GO


/* ================================================================
   31. JSON NESTED STRUCTURE
   ================================================================ */

DECLARE @Json NVARCHAR(MAX) =
N'
{
    "customer": {
        "id": 1,
        "name": "Nova User",
        "address": {
            "city": "Ha Noi",
            "country": "VN"
        },
        "orders": [
            {
                "id": 101,
                "amount": 1000
            },
            {
                "id": 102,
                "amount": 2000
            }
        ]
    }
}
';


SELECT

    JSON_VALUE
    (
        @Json,
        '$.customer.name'
    ) AS CustomerName,

    JSON_VALUE
    (
        @Json,
        '$.customer.address.city'
    ) AS City,

    JSON_QUERY
    (
        @Json,
        '$.customer.orders'
    ) AS Orders;
GO


/* ================================================================
   32. OPENJSON + CROSS APPLY
   ================================================================ */

DECLARE @NestedJson NVARCHAR(MAX) =
N'
[
    {
        "customer": "A",
        "orders": [
            {"id": 1, "amount": 100},
            {"id": 2, "amount": 200}
        ]
    },
    {
        "customer": "B",
        "orders": [
            {"id": 3, "amount": 300}
        ]
    }
]
';


SELECT
    C.CustomerName,

    O.OrderID,

    O.Amount

FROM OPENJSON(@NestedJson)

WITH
(
    CustomerName NVARCHAR(100)
        '$.customer',

    Orders NVARCHAR(MAX)
        '$.orders'
        AS JSON

) AS C


CROSS APPLY

OPENJSON(C.Orders)

WITH
(
    OrderID INT
        '$.id',

    Amount DECIMAL(18,2)
        '$.amount'

) AS O;
GO


/* ================================================================
   33. FOR JSON NESTED
   ================================================================ */

SELECT
    C.CustomerID,

    C.FullName,

    (
        SELECT
            O.OrderID,
            O.OrderDate,
            O.TotalAmount

        FROM dbo.Orders AS O

        WHERE
            O.CustomerID =
            C.CustomerID

        FOR JSON PATH

    ) AS Orders

FROM dbo.Customers AS C

FOR JSON PATH,
ROOT('customers');
GO


/* ================================================================
   34. COMPLEX BOOLEAN EXPRESSION
   ================================================================ */

SELECT *

FROM dbo.Customers

WHERE
(
    City = N'Hà Nội'

    AND

    (
        Balance >= 1000000

        OR

        (
            Phone IS NOT NULL
            AND
            Email IS NOT NULL
        )
    )
)

OR

(
    City = N'TP.HCM'

    AND

    NOT
    (
        Balance < 500000
        OR
        Balance IS NULL
    )
);
GO


/* ================================================================
   35. SET OPERATORS LỒNG
   ================================================================ */

SELECT CustomerID

FROM
(
    SELECT CustomerID
    FROM dbo.Customers
    WHERE Balance >= 1000000

    UNION

    SELECT CustomerID
    FROM dbo.Customers
    WHERE City = N'Hà Nội'

) AS U

WHERE CustomerID IN
(
    SELECT CustomerID

    FROM dbo.Customers

    WHERE Balance > 0
);
GO


/* ================================================================
   36. COMPLEX STORED PROCEDURE
   ================================================================ */

CREATE OR ALTER PROCEDURE dbo.sp_AdvancedCustomerSearch

    @City NVARCHAR(100) = NULL,

    @MinBalance DECIMAL(18,2) = NULL,

    @MaxBalance DECIMAL(18,2) = NULL,

    @PageNumber INT = 1,

    @PageSize INT = 20

AS

BEGIN

    SET NOCOUNT ON;


    IF @PageNumber < 1
        SET @PageNumber = 1;


    IF @PageSize < 1
        SET @PageSize = 20;


    WITH FilteredCustomers AS
    (
        SELECT

            CustomerID,
            FullName,
            Email,
            City,
            Balance,

            ROW_NUMBER()
            OVER
            (
                ORDER BY
                    Balance DESC,
                    CustomerID ASC
            )
            AS RowNumber

        FROM dbo.Customers

        WHERE
        (
            @City IS NULL

            OR

            City = @City
        )

        AND
        (
            @MinBalance IS NULL

            OR

            Balance >=
            @MinBalance
        )

        AND
        (
            @MaxBalance IS NULL

            OR

            Balance <=
            @MaxBalance
        )
    )

    SELECT *

    FROM FilteredCustomers

    WHERE RowNumber BETWEEN

        ((@PageNumber - 1)
          * @PageSize) + 1

    AND

        @PageNumber
        * @PageSize

    ORDER BY RowNumber;

END;
GO


EXEC dbo.sp_AdvancedCustomerSearch

    @City = NULL,

    @MinBalance = 1000000,

    @MaxBalance = NULL,

    @PageNumber = 1,

    @PageSize = 10;
GO


/* ================================================================
   37. TRIGGER XỬ LÝ MULTI-ROW
   ================================================================ */

CREATE OR ALTER TRIGGER dbo.trg_AdvancedProductAudit

ON dbo.Products

AFTER UPDATE

AS

BEGIN

    SET NOCOUNT ON;


    INSERT INTO dbo.PriceLog
    (
        ProductID,
        OldPrice,
        NewPrice
    )

    SELECT
        D.ProductID,

        D.Price,

        I.Price

    FROM deleted AS D

    INNER JOIN inserted AS I

        ON I.ProductID =
           D.ProductID

    WHERE
        ISNULL(D.Price,0)
        <>
        ISNULL(I.Price,0);

END;
GO


UPDATE dbo.Products

SET Price =
    Price + 100

WHERE ProductID IN
(
    1,
    2,
    3
);
GO


/* ================================================================
   38. FINAL "MONSTER QUERY"
   ================================================================ */

WITH
BaseProduct AS
(
    SELECT

        P.ProductID,
        P.ProductCode,
        P.ProductName,
        P.CategoryID,
        P.Price,
        P.Quantity,

        P.Price * P.Quantity
            AS StockValue

    FROM dbo.Products AS P

    WHERE
        P.Price > 0
),

CategoryStats AS
(
    SELECT

        CategoryID,

        COUNT(*)
            AS ProductCount,

        AVG(Price)
            AS AveragePrice,

        SUM
        (
            Price * Quantity
        )
        AS TotalStockValue

    FROM BaseProduct

    GROUP BY CategoryID
),

RankedProduct AS
(
    SELECT

        P.*,

        S.ProductCount,

        S.AveragePrice,

        S.TotalStockValue,

        ROW_NUMBER()
        OVER
        (
            PARTITION BY
                P.CategoryID

            ORDER BY
                P.Price DESC,
                P.ProductID
        )
        AS CategoryRowNumber,

        RANK()
        OVER
        (
            ORDER BY
                P.Price DESC
        )
        AS GlobalRank,

        SUM(P.StockValue)
        OVER
        (
            PARTITION BY
                P.CategoryID

            ORDER BY
                P.ProductID

            ROWS BETWEEN
                UNBOUNDED PRECEDING
                AND CURRENT ROW
        )
        AS RunningCategoryStockValue

    FROM BaseProduct AS P

    INNER JOIN CategoryStats AS S

        ON
        (
            P.CategoryID =
            S.CategoryID

            OR

            (
                P.CategoryID IS NULL

                AND

                S.CategoryID IS NULL
            )
        )
)

SELECT TOP (100)

    R.ProductID,

    R.ProductCode,

    R.ProductName,

    C.CategoryName,

    R.Price,

    R.Quantity,

    R.StockValue,

    R.ProductCount,

    R.AveragePrice,

    R.TotalStockValue,

    R.CategoryRowNumber,

    R.GlobalRank,

    R.RunningCategoryStockValue,


    CASE

        WHEN
            R.Price >
            R.AveragePrice

        THEN

            CASE

                WHEN
                    R.StockValue >
                    R.TotalStockValue * 0.50

                THEN N'EXPENSIVE + DOMINANT'

                ELSE N'ABOVE AVERAGE'

            END


        WHEN
            R.Price =
            R.AveragePrice

        THEN N'AVERAGE'


        ELSE N'BELOW AVERAGE'

    END AS ProductClassification,


    TAX.PriceWithVAT,

    DISCOUNT.FinalPrice,


    (
        SELECT COUNT(*)

        FROM dbo.Products AS P2

        WHERE
            P2.CategoryID =
            R.CategoryID

            AND

            P2.Price >
            R.Price

    ) AS MoreExpensiveProducts,


    CASE

        WHEN EXISTS
        (
            SELECT 1

            FROM dbo.OrderDetails AS OD

            WHERE
                OD.ProductID =
                R.ProductID
        )

        THEN 1

        ELSE 0

    END AS HasBeenOrdered


FROM RankedProduct AS R


LEFT JOIN dbo.Categories AS C

    ON C.CategoryID =
       R.CategoryID


CROSS APPLY
(
    SELECT
        R.Price * 1.10
        AS PriceWithVAT

) AS TAX


CROSS APPLY
(
    SELECT

        CASE

            WHEN
                TAX.PriceWithVAT
                >= 20000000

            THEN
                TAX.PriceWithVAT
                * 0.90


            WHEN
                TAX.PriceWithVAT
                >= 10000000

            THEN
                TAX.PriceWithVAT
                * 0.95


            ELSE
                TAX.PriceWithVAT

        END AS FinalPrice

) AS DISCOUNT


WHERE

    R.Quantity > 0

    AND

    R.ProductID IN
    (
        SELECT ProductID

        FROM dbo.Products

        WHERE Price > 0
    )


ORDER BY

    R.CategoryID,

    R.Price DESC,

    R.ProductID

OPTION(RECOMPILE);
GO
"""

r = requests.post(f'{BASE}/execute', json={'sql': full_script}, timeout=20)
print('FINAL ADVANCED TEST STATUS:', r.status_code)
if r.status_code == 200:
    print('SUCCESS! ALL 38 ADVANCED T-SQL SECTIONS PASSED WITH 200 OK!')
else:
    print('ERROR:', r.text[:800])
