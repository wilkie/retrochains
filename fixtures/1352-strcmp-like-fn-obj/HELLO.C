int eq(char *a, char *b) {
  while (*a && *a == *b) {
    a++;
    b++;
  }
  return *a - *b;
}
int main(void) {
  return eq("ab", "ab");
}
