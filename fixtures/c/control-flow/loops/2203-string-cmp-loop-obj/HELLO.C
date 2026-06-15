int str_eq(char *a, char *b) {
  while (*a && *b && *a == *b) { a++; b++; }
  return *a == *b;
}
int main(void) {
  return str_eq("hello", "hello");
}
