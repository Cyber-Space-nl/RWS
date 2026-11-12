<?php
header('Content-Type: application/json');
echo json_encode(['rws' => 'ok', 'php' => PHP_VERSION], JSON_PRETTY_PRINT);
