int eqi(int *a, int *b) {
  while (*a && *a == *b) {
    a++;
    b++;
  }
  return *a - *b;
}
