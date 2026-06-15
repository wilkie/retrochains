int eq3(char *a, char *b, char *c) {
  while (*a && *a == *b && *a == *c) {
    a++;
    b++;
    c++;
  }
  return *a - *b;
}
