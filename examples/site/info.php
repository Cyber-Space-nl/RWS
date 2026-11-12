<?php
header('Content-Type: application/json');
echo json_encode(['rsw' => 'ok', 'php' => PHP_VERSION], JSON_PRETTY_PRINT);
