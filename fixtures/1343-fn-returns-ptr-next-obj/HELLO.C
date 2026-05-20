char *nextp(char *p) {
  return p + 1;
}
int main(void) {
  return *nextp("ab");
}
