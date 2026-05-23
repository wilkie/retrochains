int eqstr(char *a, char *b) {
  while (*a && *a == *b) {
    a++;
    b++;
  }
  return *a - *b;
}
