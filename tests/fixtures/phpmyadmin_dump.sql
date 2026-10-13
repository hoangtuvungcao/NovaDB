-- phpMyAdmin SQL Dump (synthetic fixture)
-- Server version: 8.0.36
-- Generation Time: Oct 12, 2026

SET SQL_MODE = "NO_AUTO_VALUE_ON_ZERO";
START TRANSACTION;
SET time_zone = "+00:00";

/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;
/*!40101 SET @OLD_CHARACTER_SET_RESULTS=@@CHARACTER_SET_RESULTS */;
/*!40101 SET @OLD_COLLATION_CONNECTION=@@COLLATION_CONNECTION */;
/*!40101 SET NAMES utf8mb4 */;
/*!40014 SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0 */;
/*!40014 SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0 */;
/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */;
/*!40111 SET @OLD_SQL_NOTES=@@SQL_NOTES, SQL_NOTES=0 */;

--
-- Database: `shop`
--

-- --------------------------------------------------------

--
-- Table structure for table `customers`
--

CREATE TABLE `customers` (
  `id` int(11) NOT NULL,
  `name` varchar(100) COLLATE utf8mb4_unicode_ci NOT NULL,
  `email` varchar(120) COLLATE utf8mb4_unicode_ci DEFAULT NULL,
  `created_at` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

--
-- Dumping data for table `customers`
--

INSERT INTO `customers` (`id`, `name`, `email`, `created_at`) VALUES
(1, 'Nguyễn Văn An', 'an@example.com', '2026-01-01 08:00:00'),
(2, 'Trần Thị Bích', 'bich@example.com', '2026-01-02 09:30:00'),
(3, 'Lê O\'Connor', NULL, '2026-01-03 10:45:00'),
(7, 'Phạm Đình Hùng', 'hung@example.com', '2026-01-04 11:00:00');

-- --------------------------------------------------------

CREATE TABLE `orders` (
  `id` bigint(20) UNSIGNED NOT NULL,
  `customer_id` int(11) NOT NULL,
  `total` decimal(10,2) NOT NULL DEFAULT '0.00',
  `note` text COLLATE utf8mb4_unicode_ci
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

INSERT INTO `orders` (`id`, `customer_id`, `total`, `note`) VALUES
(10, 1, '150000.50', 'Giao hàng buổi sáng, \"cẩn thận\"'),
(11, 2, '99000.00', 'Đơn hàng thứ hai\nxuống dòng'),
(12, 1, '5000.00', NULL);

CREATE TABLE `order_items` (
  `order_id` bigint(20) UNSIGNED NOT NULL,
  `line` int(11) NOT NULL,
  `sku` varchar(32) NOT NULL,
  `qty` int(11) NOT NULL DEFAULT '1'
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

INSERT INTO `order_items` (`order_id`, `line`, `sku`, `qty`) VALUES
(10, 1, 'SKU-A', 2),
(10, 2, 'SKU-B', 1),
(11, 1, 'SKU-A', 5);

--
-- Indexes for dumped tables
--

--
-- Indexes for table `customers`
--
ALTER TABLE `customers`
  ADD PRIMARY KEY (`id`),
  ADD UNIQUE KEY `email` (`email`),
  ADD KEY `idx_name` (`name`);

--
-- Indexes for table `orders`
--
ALTER TABLE `orders`
  ADD PRIMARY KEY (`id`),
  ADD KEY `customer_id` (`customer_id`);

ALTER TABLE `order_items`
  ADD PRIMARY KEY (`order_id`,`line`);

--
-- AUTO_INCREMENT for dumped tables
--

ALTER TABLE `customers`
  MODIFY `id` int(11) NOT NULL AUTO_INCREMENT, AUTO_INCREMENT=100;

ALTER TABLE `orders`
  MODIFY `id` bigint(20) UNSIGNED NOT NULL AUTO_INCREMENT, AUTO_INCREMENT=13;

--
-- Constraints for dumped tables
--

ALTER TABLE `orders`
  ADD CONSTRAINT `orders_ibfk_1` FOREIGN KEY (`customer_id`) REFERENCES `customers` (`id`) ON DELETE CASCADE;

ALTER TABLE `order_items`
  ADD CONSTRAINT `items_order_fk` FOREIGN KEY (`order_id`) REFERENCES `orders` (`id`);
CREATE TABLE `board_a` (
  `id` int(11) NOT NULL,
  `username` varchar(50) NOT NULL,
  `svID` int(11) NOT NULL DEFAULT '0',
  `memo` varchar(50) DEFAULT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

INSERT INTO `board_a` (`id`, `username`, `svID`, `memo`) VALUES
(1, 'board_a_u1', 1, 'a'),
(2, 'board_a_u2', 2, 'b');

ALTER TABLE `board_a`
  ADD PRIMARY KEY (`id`) USING BTREE,
  ADD KEY `Index_2` (`username`) USING BTREE,
  ADD KEY `Index_3` (`svID`) USING BTREE,
  ADD KEY (`memo`),
  ADD UNIQUE KEY `uniq_user` (`username`, `svID`);

CREATE INDEX `Index_5` ON `board_a` (`memo`, `svID`);
CREATE UNIQUE INDEX `username` ON `board_a` (`id`, `username`);

CREATE TABLE `board_b` (
  `id` int(11) NOT NULL,
  `username` varchar(50) NOT NULL,
  `svID` int(11) NOT NULL DEFAULT '0',
  `memo` varchar(50) DEFAULT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

INSERT INTO `board_b` (`id`, `username`, `svID`, `memo`) VALUES
(1, 'board_b_u1', 1, 'a'),
(2, 'board_b_u2', 2, 'b');

ALTER TABLE `board_b`
  ADD PRIMARY KEY (`id`) USING BTREE,
  ADD KEY `Index_2` (`username`) USING BTREE,
  ADD KEY `Index_3` (`svID`) USING BTREE,
  ADD KEY (`memo`),
  ADD UNIQUE KEY `uniq_user` (`username`, `svID`);

CREATE INDEX `Index_5` ON `board_b` (`memo`, `svID`);
CREATE UNIQUE INDEX `username` ON `board_b` (`id`, `username`);

CREATE TABLE `board_c` (
  `id` int(11) NOT NULL,
  `username` varchar(50) NOT NULL,
  `svID` int(11) NOT NULL DEFAULT '0',
  `memo` varchar(50) DEFAULT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

INSERT INTO `board_c` (`id`, `username`, `svID`, `memo`) VALUES
(1, 'board_c_u1', 1, 'a'),
(2, 'board_c_u2', 2, 'b');

ALTER TABLE `board_c`
  ADD PRIMARY KEY (`id`) USING BTREE,
  ADD KEY `Index_2` (`username`) USING BTREE,
  ADD KEY `Index_3` (`svID`) USING BTREE,
  ADD KEY (`memo`),
  ADD UNIQUE KEY `uniq_user` (`username`, `svID`);

CREATE INDEX `Index_5` ON `board_c` (`memo`, `svID`);
CREATE UNIQUE INDEX `username` ON `board_c` (`id`, `username`);

/*!40000 ALTER TABLE `order_items` DISABLE KEYS */;
/*!40000 ALTER TABLE `order_items` ENABLE KEYS */;
COMMIT;

/*!40101 SET CHARACTER_SET_CLIENT=@OLD_CHARACTER_SET_CLIENT */;
/*!40101 SET CHARACTER_SET_RESULTS=@OLD_CHARACTER_SET_RESULTS */;
/*!40101 SET COLLATION_CONNECTION=@OLD_COLLATION_CONNECTION */;
/*!40014 SET UNIQUE_CHECKS=@OLD_UNIQUE_CHECKS */;
/*!40014 SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS */;
/*!40101 SET SQL_MODE=@OLD_SQL_MODE */;
/*!40111 SET SQL_NOTES=@OLD_SQL_NOTES */;
