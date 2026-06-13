int main(void) {
#ifdef FOO
  return FOO + 10;
#else
  return 0;
#endif
}
