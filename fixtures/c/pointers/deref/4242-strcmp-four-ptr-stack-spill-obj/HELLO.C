int eq4(char *a, char *b, char *c, char *d) {
  while (*a && *a == *b && *a == *c && *a == *d) { a++; b++; c++; d++; }
  return *a - *b;
}
