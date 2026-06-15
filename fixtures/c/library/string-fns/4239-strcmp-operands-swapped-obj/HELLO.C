int eqs(char *a, char *b) {
  while (*a && *b == *a) {
    a++;
    b++;
  }
  return *a - *b;
}
